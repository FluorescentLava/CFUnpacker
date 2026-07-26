using System.Diagnostics;
using System.IO.Compression;
using CarrotUnpacker.Models;

namespace CarrotUnpacker.Core;

internal sealed record ApkGameDetection(
    GameProfile? Profile,
    IReadOnlyList<GameKind> CompatibleKinds,
    string Evidence,
    TimeSpan Elapsed)
{
    public bool IsKnown => Profile is not null;

    public bool IsCompatible(GameProfile profile) =>
        CompatibleKinds.Contains(profile.Kind);
}

internal static class ApkGameDetector
{
    private const string AssetsPrefix = "assets/";
    private static readonly byte[] Carrot4Header = [0xFF, 0xDB, 0xFF, 0xEE, 0x66];

    public static Task<ApkGameDetection> DetectAsync(
        string apkPath,
        CancellationToken cancellationToken = default) =>
        Task.Run(() => Detect(apkPath, cancellationToken), cancellationToken);

    internal static ApkGameDetection Detect(
        string apkPath,
        CancellationToken cancellationToken = default)
    {
        var stopwatch = Stopwatch.StartNew();
        using var apk = new FileStream(
            apkPath,
            new FileStreamOptions
            {
                Mode = FileMode.Open,
                Access = FileAccess.Read,
                Share = FileShare.Read,
                BufferSize = 64 * 1024,
                Options = FileOptions.RandomAccess,
            });
        using var archive = new ZipArchive(apk, ZipArchiveMode.Read, leaveOpen: false);
        List<ZipArchiveEntry> assets = archive.Entries
            .Where(entry =>
                !string.IsNullOrEmpty(entry.Name) &&
                entry.FullName.StartsWith(AssetsPrefix, StringComparison.OrdinalIgnoreCase))
            .ToList();
        if (assets.Count == 0)
        {
            return Unknown("APK 中没有 assets 资源目录。", stopwatch);
        }

        cancellationToken.ThrowIfCancellationRequested();
        (bool hasCzzf, bool hasCarrot4Wrapper) = FindWrappedResources(assets, cancellationToken);
        if (hasCarrot4Wrapper)
        {
            return Known(
                GameKind.Carrot4,
                [GameKind.Carrot4],
                "检测到四代的 ff db ff ee 66 加密资源头。",
                stopwatch);
        }

        if (hasCzzf)
        {
            bool hasCarrot3Directory = HasPrefix(assets, "assets/Carrot3/");
            bool hasAboMarkers =
                HasPrefix(assets, "assets/pandora/") ||
                HasPrefix(assets, "assets/TuSDK.bundle/");
            GameKind preferred = hasAboMarkers && !hasCarrot3Directory
                ? GameKind.AboAdventure
                : GameKind.Carrot3;
            string identityEvidence = preferred == GameKind.AboAdventure
                ? "并检测到阿波之旅的特征目录。"
                : hasCarrot3Directory
                    ? "并检测到 Carrot3 特征目录。"
                    : "三代与阿波之旅共用该流程，未找到更强的名称特征。";
            return Known(
                preferred,
                [GameKind.Carrot3, GameKind.AboAdventure],
                $"检测到 czzf 加密 plist，{identityEvidence}",
                stopwatch);
        }

        (bool carrot1Key, bool carrot2Key) = ProbeLegacyCczKeys(assets, cancellationToken);
        int carrot1Score = ScoreCarrot1Layout(assets);
        int carrot2Score = ScoreCarrot2Layout(assets);
        if (carrot1Key)
        {
            return Known(
                GameKind.Carrot1,
                [GameKind.Carrot1],
                "CCZ 样本通过一代 key 解密并验证为 PVR。",
                stopwatch);
        }

        if (carrot2Key || carrot1Score >= 3 || carrot2Score >= 3)
        {
            if (carrot2Score > carrot1Score)
            {
                return Known(
                    GameKind.Carrot2,
                    [GameKind.Carrot2],
                    $"二代资源特征得分更高（{carrot2Score}:{carrot1Score}），CCZ/PVR 校验通过。",
                    stopwatch);
            }

            if (carrot1Score > carrot2Score)
            {
                return Known(
                    GameKind.Carrot1,
                    [GameKind.Carrot1],
                    $"一代资源特征得分更高（{carrot1Score}:{carrot2Score}），兼容 CCZ/PVR 校验通过。",
                    stopwatch);
            }

            if (carrot2Key)
            {
                return Known(
                    GameKind.Carrot2,
                    [GameKind.Carrot1, GameKind.Carrot2],
                    "CCZ 样本使用二代 key；一代部分版本也使用该 key，两个旧版流程均视为兼容。",
                    stopwatch);
            }
        }

        return Unknown(
            "没有检测到受支持的加密头、可验证 CCZ key 或足够的系列资源特征。",
            stopwatch);
    }

    private static (bool HasCzzf, bool HasCarrot4Wrapper) FindWrappedResources(
        IReadOnlyList<ZipArchiveEntry> assets,
        CancellationToken cancellationToken)
    {
        bool hasCzzf = false;
        IEnumerable<ZipArchiveEntry> candidates = assets
            .Where(entry =>
                entry.Name.EndsWith(".plist", StringComparison.OrdinalIgnoreCase) ||
                entry.Name.EndsWith(".png", StringComparison.OrdinalIgnoreCase))
            .OrderBy(entry =>
                entry.Name.EndsWith(".plist", StringComparison.OrdinalIgnoreCase) ? 0 : 1)
            .ThenBy(entry => entry.Length)
            .Take(64);
        Span<byte> header = stackalloc byte[5];
        foreach (ZipArchiveEntry entry in candidates)
        {
            cancellationToken.ThrowIfCancellationRequested();
            header.Clear();
            using Stream stream = entry.Open();
            int read = ReadAtMost(stream, header);
            if (read >= Carrot4Header.Length &&
                header.SequenceEqual(Carrot4Header))
            {
                return (hasCzzf, true);
            }

            if (read >= 4 && header[..4].SequenceEqual("czzf"u8))
            {
                hasCzzf = true;
            }
        }

        return (hasCzzf, false);
    }

    private static (bool Carrot1Key, bool Carrot2Key) ProbeLegacyCczKeys(
        IReadOnlyList<ZipArchiveEntry> assets,
        CancellationToken cancellationToken)
    {
        EncryptionKey carrot1Key = GameProfile.For(GameKind.Carrot1).PvrKeys[0];
        EncryptionKey carrot2Key = GameProfile.For(GameKind.Carrot2).PvrKeys[0];
        bool carrot1Match = false;
        bool carrot2Match = false;
        IEnumerable<ZipArchiveEntry> candidates = assets
            .Where(entry =>
                entry.Name.EndsWith(".pvr.ccz", StringComparison.OrdinalIgnoreCase) &&
                entry.Length is >= 16 and <= 32 * 1024 * 1024)
            .OrderBy(entry => entry.Length)
            .Take(4);
        foreach (ZipArchiveEntry entry in candidates)
        {
            cancellationToken.ThrowIfCancellationRequested();
            byte[] data = ReadEntry(entry);
            if (!EncryptionCodec.IsCcz(data) || data[3] != (byte)'p')
            {
                continue;
            }

            carrot1Match |= CanDecodeCcz(data, carrot1Key);
            carrot2Match |= CanDecodeCcz(data, carrot2Key);
            if (carrot1Match || carrot2Match)
            {
                break;
            }
        }

        return (carrot1Match, carrot2Match);
    }

    private static bool CanDecodeCcz(byte[] data, EncryptionKey key)
    {
        try
        {
            _ = EncryptionCodec.DecodeCcz(data, [key], out _);
            return true;
        }
        catch (InvalidDataException)
        {
            return false;
        }
    }

    private static int ScoreCarrot1Layout(IReadOnlyList<ZipArchiveEntry> assets)
    {
        int score = 0;
        score += HasExtensionUnder(assets, "assets/Music/", ".ogg") ? 3 : 0;
        score += HasPrefix(assets, "assets/Themes/scene/") ? 2 : 0;
        score += HasEntry(assets, "assets/Themes/Items/CommonTip.plist") ? 2 : 0;
        return score;
    }

    private static int ScoreCarrot2Layout(IReadOnlyList<ZipArchiveEntry> assets)
    {
        int score = 0;
        score += HasPrefix(assets, "assets/Themes_cn/") ? 3 : 0;
        score += HasPrefix(assets, "assets/Themes/DayMaps/") ? 2 : 0;
        score += HasExtensionUnder(assets, "assets/Music/", ".mp3") ? 2 : 0;
        score += HasEntry(assets, "assets/Info.plist") ? 1 : 0;
        return score;
    }

    private static bool HasEntry(IReadOnlyList<ZipArchiveEntry> entries, string fullName) =>
        entries.Any(entry =>
            string.Equals(entry.FullName, fullName, StringComparison.OrdinalIgnoreCase));

    private static bool HasPrefix(IReadOnlyList<ZipArchiveEntry> entries, string prefix) =>
        entries.Any(entry =>
            entry.FullName.StartsWith(prefix, StringComparison.OrdinalIgnoreCase));

    private static bool HasExtensionUnder(
        IReadOnlyList<ZipArchiveEntry> entries,
        string prefix,
        string extension) =>
        entries.Any(entry =>
            entry.FullName.StartsWith(prefix, StringComparison.OrdinalIgnoreCase) &&
            entry.Name.EndsWith(extension, StringComparison.OrdinalIgnoreCase));

    private static byte[] ReadEntry(ZipArchiveEntry entry)
    {
        using Stream stream = entry.Open();
        using var destination = new MemoryStream(checked((int)entry.Length));
        stream.CopyTo(destination);
        return destination.ToArray();
    }

    private static int ReadAtMost(Stream stream, Span<byte> destination)
    {
        int total = 0;
        while (total < destination.Length)
        {
            int read = stream.Read(destination[total..]);
            if (read == 0)
            {
                break;
            }

            total += read;
        }

        return total;
    }

    private static ApkGameDetection Known(
        GameKind preferred,
        IReadOnlyList<GameKind> compatibleKinds,
        string evidence,
        Stopwatch stopwatch)
    {
        stopwatch.Stop();
        return new ApkGameDetection(
            GameProfile.For(preferred),
            compatibleKinds,
            evidence,
            stopwatch.Elapsed);
    }

    private static ApkGameDetection Unknown(string evidence, Stopwatch stopwatch)
    {
        stopwatch.Stop();
        return new ApkGameDetection(null, [], evidence, stopwatch.Elapsed);
    }
}
