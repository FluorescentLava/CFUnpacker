using System.IO.Compression;
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using CFUnpacker.Core;
using CFUnpacker.Models;
using SkiaSharp;

string seriesRoot = Path.Combine(
    Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory),
    "保卫萝卜系列解包");
string temporaryRoot = Path.Combine(Path.GetTempPath(), $"CFUnpacker-verify-{Guid.NewGuid():N}");
Directory.CreateDirectory(temporaryRoot);

if (args.Length >= 2 &&
    string.Equals(args[0], "--detect", StringComparison.OrdinalIgnoreCase))
{
    ApkGameDetection detection = await ApkGameDetector.DetectAsync(args[1]);
    Console.WriteLine(
        $"DETECT profile={detection.Profile?.DisplayName ?? "未知"} | " +
        $"compatible={string.Join(',', detection.CompatibleKinds)} | " +
        $"elapsed={detection.Elapsed.TotalMilliseconds:F1}ms | {detection.Evidence}");
    return;
}

if (args.Contains("--bench-split", StringComparer.OrdinalIgnoreCase))
{
    int workerCount = int.TryParse(args.LastOrDefault(), out int parsedWorkers)
        ? parsedWorkers
        : Math.Clamp(Environment.ProcessorCount - 2, 1, 12);
    RunSplitBenchmark(seriesRoot, temporaryRoot, workerCount);
    return;
}

VerifyPvrV3Bgr565ChannelOrder();
if (args.Contains("--pvr-regression", StringComparer.OrdinalIgnoreCase))
{
    return;
}

var samples = new[]
{
    new Sample(GameKind.Carrot1, "保卫萝卜", @"Themes\Items\Common.plist"),
    new Sample(GameKind.Carrot1, "保卫萝卜", @"Themes\scene\mainscene-hd.plist"),
    new Sample(GameKind.Carrot2, "保卫萝卜2", @"Themes\DayMaps\BG1_1_1.plist"),
    new Sample(GameKind.Carrot3, "保卫萝卜3", @"Themes\AqiuTheme\AqiuCarrot.plist"),
    new Sample(GameKind.Carrot4, "保卫萝卜4", @"res\active\active_christmas_popupdlg.plist"),
    new Sample(GameKind.Carrot4, "保卫萝卜4", @"res\house\1000\100002\dollhouse_abo_angel_clothes.plist"),
    new Sample(GameKind.AboAdventure, "保卫萝卜阿波之旅", @"Themes\CarrotToy\CarrorBadge.plist"),
};

try
{
    foreach (Sample sample in samples)
    {
        string gameRoot = Path.Combine(seriesRoot, sample.Folder);
        string plistPath = Path.Combine(gameRoot, sample.RelativePlist);
        GameProfile profile = GameProfile.For(sample.Kind);
        AtlasDefinition definition = PlistAtlasReader.Read(plistPath, profile) ??
                                     throw new Exception($"No frames: {sample.RelativePlist}");
        string outputRoot = Path.Combine(temporaryRoot, sample.Folder);
        AtlasSplitResult result = AtlasSplitter.Split(
            plistPath,
            gameRoot,
            outputRoot,
            profile,
            CancellationToken.None);

        AtlasFrame first = definition.Frames[0];
        string relativeDirectory = Path.GetDirectoryName(sample.RelativePlist) ?? string.Empty;
        string atlasName = Path.GetFileNameWithoutExtension(sample.RelativePlist);
        string generated = Path.Combine(
            outputRoot,
            relativeDirectory,
            atlasName,
            first.Name.Replace('/', Path.DirectorySeparatorChar));
        string expected = Path.Combine(
            gameRoot,
            "Unpacked_PNG",
            relativeDirectory,
            atlasName,
            first.Name.Replace('/', Path.DirectorySeparatorChar));
        CompareBitmaps(expected, generated);
        Console.WriteLine(
            $"PASS {sample.Folder} | {sample.RelativePlist} | " +
            $"{result.FramesWritten} frames | {result.TextureKey} | {result.PlistTransform}");
    }

    await VerifyDetectionAsync(seriesRoot, temporaryRoot);
    await VerifyEndToEndAsync(seriesRoot, temporaryRoot);
    if (args.Contains("--full", StringComparer.OrdinalIgnoreCase))
    {
        RunFullScan(seriesRoot);
    }
}
finally
{
    if (Directory.Exists(temporaryRoot))
    {
        Directory.Delete(temporaryRoot, recursive: true);
    }
}

static void CompareBitmaps(string expectedPath, string actualPath)
{
    using SKBitmap expected = SKBitmap.Decode(expectedPath) ??
                              throw new Exception($"Cannot decode expected image: {expectedPath}");
    using SKBitmap actual = SKBitmap.Decode(actualPath) ??
                            throw new Exception($"Cannot decode generated image: {actualPath}");
    if (expected.Width != actual.Width || expected.Height != actual.Height)
    {
        throw new Exception(
            $"Size mismatch: expected {expected.Width}x{expected.Height}, actual {actual.Width}x{actual.Height}");
    }

    byte[] expectedPixels = CopyPixels(expected);
    byte[] actualPixels = CopyPixels(actual);
    if (!expectedPixels.AsSpan().SequenceEqual(actualPixels))
    {
        int mismatch = expectedPixels
            .Zip(actualPixels)
            .TakeWhile(pair => pair.First == pair.Second)
            .Count();
        string expectedHead = Convert.ToHexString(expectedPixels.AsSpan(0, Math.Min(16, expectedPixels.Length)));
        string actualHead = Convert.ToHexString(actualPixels.AsSpan(0, Math.Min(16, actualPixels.Length)));
        byte[] expectedStraightPixels = DecodeUnpremul(expectedPath);
        byte[] actualStraightPixels = DecodeUnpremul(actualPath);
        int contextStart = Math.Max(0, mismatch - 8);
        int contextLength = Math.Min(24, expectedPixels.Length - contextStart);
        string expectedContext = Convert.ToHexString(expectedPixels.AsSpan(contextStart, contextLength));
        string actualContext = Convert.ToHexString(actualPixels.AsSpan(contextStart, contextLength));
        string expectedStraight = Convert.ToHexString(expectedStraightPixels.AsSpan(contextStart, contextLength));
        string actualStraight = Convert.ToHexString(actualStraightPixels.AsSpan(contextStart, contextLength));
        string boundsExpected = AlphaBounds(expectedStraightPixels, expected.Width, expected.Height);
        string boundsActual = AlphaBounds(actualStraightPixels, actual.Width, actual.Height);
        throw new Exception(
            $"Pixel mismatch at byte {mismatch}: {expectedPath}; " +
            $"head expected={expectedHead} actual={actualHead}; " +
            $"expected={expectedContext} ({expected.ColorType}/{expected.AlphaType}); " +
            $"actual={actualContext} ({actual.ColorType}/{actual.AlphaType}); " +
            $"straight expected={expectedStraight} actual={actualStraight}; " +
            $"bounds expected={boundsExpected} actual={boundsActual}");
    }
}

static void VerifyPvrV3Bgr565ChannelOrder()
{
    byte[] pvr = new byte[54];
    BinaryPrimitives.WriteUInt32LittleEndian(pvr, 0x03525650);
    BinaryPrimitives.WriteUInt64LittleEndian(pvr.AsSpan(8), 0x0005060500626772UL);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(24), 1);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(28), 1);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(32), 1);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(36), 1);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(40), 1);
    BinaryPrimitives.WriteUInt32LittleEndian(pvr.AsSpan(44), 1);
    BinaryPrimitives.WriteUInt16LittleEndian(pvr.AsSpan(52), 0xF800);

    using SKBitmap bitmap = PvrDecoder.Decode(pvr);
    SKColor pixel = bitmap.GetPixel(0, 0);
    if (pixel.Red != 255 || pixel.Green != 0 || pixel.Blue != 0 || pixel.Alpha != 255)
    {
        throw new Exception(
            $"PVR v3 rgb/565 color regression: expected RGBA(255,0,0,255), actual {pixel}.");
    }

    Console.WriteLine("PASS PVR v3 BGR565 channel order");
}

static byte[] CopyPixels(SKBitmap bitmap)
{
    int byteCount = checked(bitmap.RowBytes * bitmap.Height);
    byte[] pixels = new byte[byteCount];
    Marshal.Copy(bitmap.GetPixels(), pixels, 0, byteCount);
    return pixels;
}

static byte[] DecodeUnpremul(string path)
{
    using FileStream stream = File.OpenRead(path);
    using SKCodec codec = SKCodec.Create(stream) ?? throw new Exception("Cannot create codec");
    var info = new SKImageInfo(codec.Info.Width, codec.Info.Height, SKColorType.Rgba8888, SKAlphaType.Unpremul);
    byte[] pixels = new byte[info.BytesSize];
    SKCodecResult result = codec.GetPixels(info, pixels);
    if (result != SKCodecResult.Success)
    {
        throw new Exception($"Codec result: {result}");
    }

    return pixels;
}

static string AlphaBounds(byte[] rgba, int width, int height)
{
    int left = width;
    int top = height;
    int right = -1;
    int bottom = -1;
    for (int y = 0; y < height; y++)
    {
        for (int x = 0; x < width; x++)
        {
            if (rgba[(y * width + x) * 4 + 3] == 0)
            {
                continue;
            }

            left = Math.Min(left, x);
            top = Math.Min(top, y);
            right = Math.Max(right, x);
            bottom = Math.Max(bottom, y);
        }
    }

    return right < 0 ? "empty" : $"{left},{top}-{right},{bottom}";
}

static async Task VerifyEndToEndAsync(string seriesRoot, string temporaryRoot)
{
    const string relativePlist = @"Themes\DayMaps\BG1_1_1.plist";
    string sourceRoot = Path.Combine(seriesRoot, "保卫萝卜2");
    string sourcePlist = Path.Combine(sourceRoot, relativePlist);
    string sourceTexture = Path.ChangeExtension(sourcePlist, ".pvr.ccz");
    string apkPath = Path.Combine(temporaryRoot, "端到端样本.apk");
    string outputParent = Path.Combine(temporaryRoot, "output");

    using (var archive = ZipFile.Open(apkPath, ZipArchiveMode.Create))
    {
        archive.CreateEntryFromFile(
            sourcePlist,
            $"assets/{relativePlist.Replace('\\', '/')}",
            CompressionLevel.NoCompression);
        archive.CreateEntryFromFile(
            sourceTexture,
            $"assets/{Path.ChangeExtension(relativePlist, ".pvr.ccz").Replace('\\', '/')}",
            CompressionLevel.NoCompression);
    }

    var unpacker = new ApkUnpacker();
    UnpackResult result = await unpacker.UnpackAsync(
        new UnpackRequest(GameProfile.For(GameKind.Carrot2), apkPath, outputParent, false),
        progress: null,
        CancellationToken.None);

    string generated = Path.Combine(
        result.OutputPath,
        "Unpacked_PNG",
        "Themes",
        "DayMaps",
        "BG1_1_1",
        "BG1_1_1.png");
    string expected = Path.Combine(
        sourceRoot,
        "Unpacked_PNG",
        "Themes",
        "DayMaps",
        "BG1_1_1",
        "BG1_1_1.png");
    CompareBitmaps(expected, generated);

    string readme = await File.ReadAllTextAsync(Path.Combine(result.OutputPath, "README.md"));
    if (!readme.Contains("熔萤FluorescentLava", StringComparison.Ordinal) ||
        !readme.Contains("AI 工具", StringComparison.Ordinal) ||
        Directory.Exists(Path.Combine(result.OutputPath, "Unpacked_PNG", "_atlases")) ||
        Directory.Exists(Path.Combine(result.OutputPath, "Unpacked_PNG", "_pvr")) ||
        Directory.EnumerateDirectories(outputParent, ".*.unpacking-*").Any())
    {
        throw new Exception("端到端输出说明或中间文件清理验证失败。");
    }

    Console.WriteLine(
        $"PASS 端到端 APK | assets={result.AssetsExtracted} | " +
        $"atlases={result.AtlasesDecoded} | frames={result.FramesWritten}");
}

static async Task VerifyDetectionAsync(string seriesRoot, string temporaryRoot)
{
    string detectionRoot = Path.Combine(temporaryRoot, "detection");
    Directory.CreateDirectory(detectionRoot);

    string carrot1Apk = Path.Combine(detectionRoot, "carrot1.apk");
    using (var archive = ZipFile.Open(carrot1Apk, ZipArchiveMode.Create))
    {
        archive.CreateEntryFromFile(
            Path.Combine(seriesRoot, "保卫萝卜", "Themes", "Items", "Common.pvr.ccz"),
            "assets/Themes/Items/Common.pvr.ccz",
            CompressionLevel.NoCompression);
        WriteEntry(archive, "assets/Music/Items/test.ogg", [0]);
    }

    string carrot2Apk = Path.Combine(detectionRoot, "carrot2.apk");
    using (var archive = ZipFile.Open(carrot2Apk, ZipArchiveMode.Create))
    {
        archive.CreateEntryFromFile(
            Path.Combine(seriesRoot, "保卫萝卜2", "Themes", "DayMaps", "BG1_1_1.pvr.ccz"),
            "assets/Themes/DayMaps/BG1_1_1.pvr.ccz",
            CompressionLevel.NoCompression);
        WriteEntry(archive, "assets/Themes_cn/marker.dat", [0]);
    }

    string carrot3Apk = Path.Combine(detectionRoot, "carrot3.apk");
    using (var archive = ZipFile.Open(carrot3Apk, ZipArchiveMode.Create))
    {
        WriteEntry(archive, "assets/Carrot3/Test.plist", "czzf-test"u8);
    }

    string carrot4Apk = Path.Combine(detectionRoot, "carrot4.apk");
    using (var archive = ZipFile.Open(carrot4Apk, ZipArchiveMode.Create))
    {
        WriteEntry(archive, "assets/res/Test.plist", [0xFF, 0xDB, 0xFF, 0xEE, 0x66]);
    }

    string aboApk = Path.Combine(detectionRoot, "abo.apk");
    using (var archive = ZipFile.Open(aboApk, ZipArchiveMode.Create))
    {
        WriteEntry(archive, "assets/Themes/Test.plist", "czzf-test"u8);
        WriteEntry(archive, "assets/pandora/marker.dat", [0]);
    }

    string unknownApk = Path.Combine(detectionRoot, "unknown.apk");
    using (var archive = ZipFile.Open(unknownApk, ZipArchiveMode.Create))
    {
        WriteEntry(archive, "assets/random.dat", [1, 2, 3]);
    }

    var expected = new[]
    {
        (carrot1Apk, GameKind.Carrot1),
        (carrot2Apk, GameKind.Carrot2),
        (carrot3Apk, GameKind.Carrot3),
        (carrot4Apk, GameKind.Carrot4),
        (aboApk, GameKind.AboAdventure),
    };
    foreach ((string apk, GameKind kind) in expected)
    {
        ApkGameDetection detection = await ApkGameDetector.DetectAsync(apk);
        if (detection.Profile?.Kind != kind)
        {
            throw new Exception(
                $"Detection mismatch for {Path.GetFileName(apk)}: " +
                $"{detection.Profile?.Kind.ToString() ?? "unknown"}");
        }
    }

    ApkGameDetection unknown = await ApkGameDetector.DetectAsync(unknownApk);
    if (unknown.IsKnown)
    {
        throw new Exception("Unknown APK was incorrectly recognized.");
    }

    Console.WriteLine("PASS APK 自动识别 | CF1/CF2/CF3/CF4/Abo/unknown");
}

static void WriteEntry(ZipArchive archive, string name, ReadOnlySpan<byte> data)
{
    ZipArchiveEntry entry = archive.CreateEntry(name, CompressionLevel.NoCompression);
    using Stream stream = entry.Open();
    stream.Write(data);
}

static void RunFullScan(string seriesRoot)
{
    var games = new[]
    {
        (GameKind.Carrot1, Folder: "保卫萝卜"),
        (GameKind.Carrot2, Folder: "保卫萝卜2"),
        (GameKind.Carrot3, Folder: "保卫萝卜3"),
        (GameKind.Carrot4, Folder: "保卫萝卜4"),
        (GameKind.AboAdventure, Folder: "保卫萝卜阿波之旅"),
    };

    foreach ((GameKind kind, string folder) in games)
    {
        string root = Path.Combine(seriesRoot, folder);
        GameProfile profile = GameProfile.For(kind);
        string[] plists = Directory
            .EnumerateFiles(root, "*.plist", SearchOption.AllDirectories)
            .Where(path => !IsUnderOutput(path, root))
            .ToArray();
        int atlasPlists = 0;
        int customTextures = 0;
        var errors = new List<string>();

        foreach (string plist in plists)
        {
            try
            {
                AtlasDefinition? definition = PlistAtlasReader.Read(plist, profile);
                if (definition is null)
                {
                    continue;
                }

                atlasPlists++;
                string png = Path.ChangeExtension(plist, ".png");
                if (kind == GameKind.Carrot4 &&
                    File.Exists(png) &&
                    IsCustomWrapped(png))
                {
                    byte[] source = File.ReadAllBytes(png);
                    if (profile.PlistKey is null ||
                        !EncryptionCodec.TryDecodeCustom(
                            source,
                            profile.PlistKey,
                            profile.PlistKeyRounds,
                            out byte[] decoded,
                            out _))
                    {
                        throw new InvalidDataException("四代 PNG 封装解密失败。");
                    }

                    using SKBitmap bitmap = SKBitmap.Decode(decoded) ??
                                            throw new InvalidDataException("四代 PNG 解码失败。");
                    customTextures++;
                }
            }
            catch (Exception exception)
            {
                errors.Add($"{Path.GetRelativePath(root, plist)}: {exception.Message}");
            }
        }

        int pvrCount = 0;
        foreach (string ccz in Directory
                     .EnumerateFiles(root, "*.pvr.ccz", SearchOption.AllDirectories)
                     .Where(path => !IsUnderOutput(path, root)))
        {
            try
            {
                byte[] pvr = EncryptionCodec.DecodeCcz(
                    File.ReadAllBytes(ccz),
                    profile.PvrKeys,
                    out _);
                using SKBitmap bitmap = PvrDecoder.Decode(pvr, profile.UnpremultiplyPvrAlpha);
                pvrCount++;
            }
            catch (Exception exception)
            {
                errors.Add($"{Path.GetRelativePath(root, ccz)}: {exception.Message}");
            }
        }

        if (errors.Count > 0)
        {
            throw new Exception(
                $"全量扫描 {folder} 失败 {errors.Count} 项：{Environment.NewLine}" +
                string.Join(Environment.NewLine, errors.Take(20)));
        }

        Console.WriteLine(
            $"PASS 全量 {folder} | plist={plists.Length} | atlases={atlasPlists} | " +
            $"pvr={pvrCount} | encrypted-png={customTextures}");
    }
}

static void RunSplitBenchmark(string seriesRoot, string temporaryRoot, int workerCount)
{
    string sourceRoot = Path.Combine(seriesRoot, "保卫萝卜3");
    string outputRoot = Path.Combine(temporaryRoot, "split-benchmark");
    string[] plists = Directory
        .EnumerateFiles(sourceRoot, "*.plist", SearchOption.AllDirectories)
        .Where(path => !IsUnderOutput(path, sourceRoot))
        .ToArray();
    var options = new ParallelOptions
    {
        MaxDegreeOfParallelism = workerCount,
    };
    int frames = 0;
    int atlases = 0;
    var stopwatch = System.Diagnostics.Stopwatch.StartNew();
    Parallel.ForEach(
        plists,
        options,
        plist =>
        {
            try
            {
                AtlasSplitResult result = AtlasSplitter.Split(
                    plist,
                    sourceRoot,
                    outputRoot,
                    GameProfile.For(GameKind.Carrot3),
                    CancellationToken.None);
                Interlocked.Add(ref frames, result.FramesWritten);
                Interlocked.Increment(ref atlases);
            }
            catch (Exception exception) when (
                exception is InvalidDataException or IOException or NotSupportedException)
            {
            }
        });
    stopwatch.Stop();
    Console.WriteLine(
        $"BENCH split | workers={workerCount} | atlases={atlases:N0} | frames={frames:N0} | " +
        $"elapsed={stopwatch.Elapsed.TotalSeconds:F3}s | fps={frames / stopwatch.Elapsed.TotalSeconds:F1}");
    Directory.Delete(outputRoot, recursive: true);
}

static bool IsUnderOutput(string path, string root)
{
    string relative = Path.GetRelativePath(root, path);
    return relative.StartsWith(
        $"Unpacked_PNG{Path.DirectorySeparatorChar}",
        StringComparison.OrdinalIgnoreCase);
}

static bool IsCustomWrapped(string path)
{
    Span<byte> header = stackalloc byte[5];
    using FileStream stream = File.OpenRead(path);
    return stream.Read(header) == header.Length &&
           header.SequenceEqual(new byte[] { 0xFF, 0xDB, 0xFF, 0xEE, 0x66 });
}

internal sealed record Sample(GameKind Kind, string Folder, string RelativePlist);
