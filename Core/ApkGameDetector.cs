using System.Diagnostics;
using System.IO.Compression;
using CFUnpacker.Models;

namespace CFUnpacker.Core;

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
        using var bundle = ApkBundle.Resolve(apkPath);
        var opened = bundle.OpenArchives();
        try
        {
            List<ZipArchiveEntry> assets = opened
                .SelectMany(source => source.Archive.Entries)
                .Where(entry =>
                    !string.IsNullOrEmpty(entry.Name) &&
                    entry.FullName.StartsWith(AssetsPrefix, StringComparison.OrdinalIgnoreCase))
                .ToList();
            if (assets.Count == 0)
            {
                return Unknown("APK 中没有 assets 资源目录。", stopwatch);
            }

            cancellationToken.ThrowIfCancellationRequested();
            return DetectFromAssets(assets, stopwatch, cancellationToken);
        }
        finally
        {
            foreach ((Stream ownerStream, ZipArchive archive) in opened)
            {
                archive.Dispose();
                ownerStream.Dispose();
            }
        }
    }

    private static ApkGameDetection DetectFromAssets(
        IReadOnlyList<ZipArchiveEntry> assets,
        Stopwatch stopwatch,
        CancellationToken cancellationToken)
    {
        (bool hasCzzf, ZipArchiveEntry? wrappedSample) = FindWrappedResources(assets, cancellationToken);
        if (wrappedSample is not null)
        {
            // 四代（含国际版）共用 ff db ff ee 66 封装与 key，用样本文件验证 key。
            bool keyVerified = CanDecodeWrapped(
                ReadEntry(wrappedSample),
                GameProfile.For(GameKind.Carrot4).PlistKey!,
                8);
            return Known(
                GameKind.Carrot4,
                [GameKind.Carrot4],
                keyVerified
                    ? "检测到四代的 ff db ff ee 66 加密资源头，样本通过四代 key 解密（国际版共用该 key）。"
                    : "检测到四代的 ff db ff ee 66 加密资源头（样本 key 校验未通过，按四代处理）。",
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

        Dictionary<GameKind, bool> cczMatches = ProbeLegacyCczKeys(assets, cancellationToken);
        int carrot1Score = ScoreCarrot1Layout(assets);
        int carrot2Score = ScoreCarrot2Layout(assets);
        int carrot4Score = ScoreCarrot4Layout(assets);
        if (cczMatches.GetValueOrDefault(GameKind.Carrot1))
        {
            return Known(
                GameKind.Carrot1,
                [GameKind.Carrot1],
                "CCZ 样本通过一代 key 解密并验证为 PVR。",
                stopwatch);
        }

        if (cczMatches.GetValueOrDefault(GameKind.Carrot2))
        {
            return Known(
                GameKind.Carrot2,
                [GameKind.Carrot1, GameKind.Carrot2],
                "CCZ 样本使用二代 key；一代部分版本也使用该 key，两个旧版流程均视为兼容。",
                stopwatch);
        }

        // 四代 1.0.0 这类早期版本可能还没有 ff db ff ee 66 封装，
        // 但 CCZp 已经使用三代/四代 key，这里直接按 key 归类。
        if (cczMatches.GetValueOrDefault(GameKind.Carrot4))
        {
            return Known(
                GameKind.Carrot4,
                [GameKind.Carrot4, GameKind.Carrot3],
                "CCZ 样本通过四代 key 解密并验证为 PVR。",
                stopwatch);
        }

        if (cczMatches.GetValueOrDefault(GameKind.Carrot3))
        {
            return Known(
                GameKind.Carrot3,
                [GameKind.Carrot3, GameKind.AboAdventure],
                "CCZ 样本通过三代 key 解密并验证为 PVR。",
                stopwatch);
        }

        if (carrot1Score >= 3 || carrot2Score >= 3 || carrot4Score >= 3)
        {
            if (carrot4Score > carrot1Score && carrot4Score > carrot2Score)
            {
                return Known(
                    GameKind.Carrot4,
                    [GameKind.Carrot4],
                    $"四代资源特征得分更高（{carrot4Score}:{carrot1Score}/{carrot2Score}），按四代流程处理。",
                    stopwatch);
            }

            if (carrot2Score > carrot1Score)
            {
                return Known(
                    GameKind.Carrot2,
                    [GameKind.Carrot2],
                    $"二代资源特征得分更高（{carrot2Score}:{carrot1Score}），CCZ/PVR 校验通过。",
                    stopwatch);
            }

            return Known(
                GameKind.Carrot1,
                [GameKind.Carrot1],
                $"一代资源特征得分更高（{carrot1Score}:{carrot2Score}），兼容 CCZ/PVR 校验通过。",
                stopwatch);
        }

        return Unknown(
            "没有检测到受支持的加密头、可验证 CCZ key 或足够的系列资源特征。",
            stopwatch);
    }

    private static (bool HasCzzf, ZipArchiveEntry? WrappedSample) FindWrappedResources(
        IReadOnlyList<ZipArchiveEntry> assets,
        CancellationToken cancellationToken)
    {
        bool hasCzzf = false;
        ZipArchiveEntry? smallestWrapped = null;
        IEnumerable<ZipArchiveEntry> Sample(string extension, int limit) => assets
            .Where(entry => entry.Name.EndsWith(extension, StringComparison.OrdinalIgnoreCase))
            .OrderBy(entry => entry.Length)
            .Take(limit);

        // plist 和 PNG 分开采样：四代早期版本 plist 可能是明文，加密只在 PNG 上。
        Span<byte> header = stackalloc byte[5];
        foreach (ZipArchiveEntry entry in Sample(".plist", 48).Concat(Sample(".png", 48)))
        {
            cancellationToken.ThrowIfCancellationRequested();
            header.Clear();
            using Stream stream = entry.Open();
            int read = ReadAtMost(stream, header);
            if (read >= Carrot4Header.Length &&
                header.SequenceEqual(Carrot4Header))
            {
                if (smallestWrapped is null || entry.Length < smallestWrapped.Length)
                {
                    smallestWrapped = entry;
                }

                continue;
            }

            if (read >= 4 && header[..4].SequenceEqual("czzf"u8))
            {
                hasCzzf = true;
            }
        }

        return (hasCzzf, smallestWrapped);
    }

    private static bool CanDecodeWrapped(byte[] data, EncryptionKey key, int rounds) =>
        EncryptionCodec.TryDecodeCustom(data, key, rounds, out _, out _);

    private static Dictionary<GameKind, bool> ProbeLegacyCczKeys(
        IReadOnlyList<ZipArchiveEntry> assets,
        CancellationToken cancellationToken)
    {
        var probes = new List<(GameKind Kind, EncryptionKey Key)>
        {
            (GameKind.Carrot1, GameProfile.For(GameKind.Carrot1).PvrKeys[0]),
            (GameKind.Carrot2, GameProfile.For(GameKind.Carrot2).PvrKeys[0]),
            (GameKind.Carrot4, GameProfile.For(GameKind.Carrot4).PvrKeys[0]),
            (GameKind.Carrot3, GameProfile.For(GameKind.Carrot3).PvrKeys[0]),
        };

        var matches = new Dictionary<GameKind, bool>();
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

            foreach ((GameKind kind, EncryptionKey key) in probes)
            {
                if (matches.GetValueOrDefault(kind))
                {
                    continue;
                }

                if (CanDecodeCcz(data, key))
                {
                    matches[kind] = true;
                }
            }

            if (matches.ContainsValue(true))
            {
                break;
            }
        }

        return matches;
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

    private static int ScoreCarrot4Layout(IReadOnlyList<ZipArchiveEntry> assets)
    {
        int score = 0;
        score += HasPrefix(assets, "assets/res/") ? 3 : 0;
        score += HasExtensionUnder(assets, "assets/res/", ".ExportJson") ? 2 : 0;
        return score;
    }

    private static bool HasEntry(IReadOnlyList<ZipArchiveEntry> entries, string fullName) =>
        entries.Any(entry =>
            string.Equals(entry.FullName, fullName, StringComparison.OrdinalIgnoreCase));

    private static bool HasPrefix(IReadOnlyList<ZipArchiveEntry> entries, string prefix) =>
        entries.Any(entry => entry.FullName.StartsWith(prefix, StringComparison.OrdinalIgnoreCase));

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
