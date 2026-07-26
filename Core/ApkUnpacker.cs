using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO.Compression;
using System.Text;
using CarrotUnpacker.Models;

namespace CarrotUnpacker.Core;

public sealed class ApkUnpacker
{
    private const string AssetsPrefix = "assets/";

    private sealed record AssetEntry(string Name, string OutputPath);

    public async Task<UnpackResult> UnpackAsync(
        UnpackRequest request,
        IProgress<UnpackProgress>? progress,
        CancellationToken cancellationToken)
    {
        ValidateRequest(request);
        var stopwatch = Stopwatch.StartNew();
        var stats = new MutableUnpackStats();
        string apkStem = Path.GetFileNameWithoutExtension(request.ApkPath);
        string outputParent = Path.GetFullPath(request.OutputParent);
        string finalPath = Path.Combine(outputParent, apkStem);
        string stagingPath = Path.Combine(outputParent, $".{apkStem}.unpacking-{Guid.NewGuid():N}");

        Directory.CreateDirectory(outputParent);
        if (File.Exists(finalPath) ||
            (Directory.Exists(finalPath) && !request.OverwriteExisting))
        {
            throw new IOException($"输出目录已存在：{finalPath}");
        }

        progress?.Report(new UnpackProgress(UnpackStage.Preparing, 0, "正在检查 APK…"));

        try
        {
            Directory.CreateDirectory(stagingPath);
            await ExtractAssetsAsync(
                request.ApkPath,
                stagingPath,
                stats,
                progress,
                cancellationToken).ConfigureAwait(false);

            cancellationToken.ThrowIfCancellationRequested();
            progress?.Report(new UnpackProgress(UnpackStage.Scanning, 38, "正在扫描 plist 和纹理…"));
            string[] plists = Directory
                .EnumerateFiles(stagingPath, "*.plist", SearchOption.AllDirectories)
                .ToArray();
            stats.PlistsScanned = plists.Length;

            string pngRoot = Path.Combine(stagingPath, "Unpacked_PNG");
            Directory.CreateDirectory(pngRoot);
            await SplitAtlasesAsync(
                plists,
                stagingPath,
                pngRoot,
                request.Profile,
                stats,
                progress,
                cancellationToken).ConfigureAwait(false);

            cancellationToken.ThrowIfCancellationRequested();
            progress?.Report(new UnpackProgress(UnpackStage.Finalizing, 96, "正在写入说明并清理中间步骤…"));
            stopwatch.Stop();
            await WriteDocumentationAsync(
                stagingPath,
                request,
                stats,
                stopwatch.Elapsed,
                cancellationToken).ConfigureAwait(false);

            cancellationToken.ThrowIfCancellationRequested();
            ReplaceOutputDirectory(stagingPath, finalPath, request.OverwriteExisting);

            progress?.Report(new UnpackProgress(UnpackStage.Completed, 100, "解包完成。"));
            return new UnpackResult(
                finalPath,
                stats.AssetsExtracted,
                stats.PlistsScanned,
                stats.AtlasesDecoded,
                stats.FramesWritten,
                stats.SkippedItems,
                stopwatch.Elapsed,
                stats.Warnings.ToArray());
        }
        catch
        {
            TryDeleteOwnedDirectory(stagingPath, outputParent);
            throw;
        }
    }

    private static async Task ExtractAssetsAsync(
        string apkPath,
        string stagingPath,
        MutableUnpackStats stats,
        IProgress<UnpackProgress>? progress,
        CancellationToken cancellationToken)
    {
        List<string> entryNames;
        await using (FileStream apk = OpenApk(apkPath))
        using (var archive = new ZipArchive(apk, ZipArchiveMode.Read, leaveOpen: false))
        {
            entryNames = archive.Entries
                .Where(entry =>
                    !string.IsNullOrEmpty(entry.Name) &&
                    entry.FullName.StartsWith(AssetsPrefix, StringComparison.OrdinalIgnoreCase))
                .Select(entry => entry.FullName)
                .Distinct(StringComparer.Ordinal)
                .ToList();
        }

        if (entryNames.Count == 0)
        {
            throw new InvalidDataException("APK 中没有 assets 资源目录。");
        }

        List<AssetEntry> entries = entryNames
            .Select(name => new AssetEntry(
                name,
                ResolveZipOutputPath(stagingPath, name[AssetsPrefix.Length..])))
            .ToList();
        foreach (string directory in entries
                     .Select(entry => Path.GetDirectoryName(entry.OutputPath)!)
                     .Distinct(StringComparer.OrdinalIgnoreCase))
        {
            Directory.CreateDirectory(directory);
        }

        int completed = 0;
        int workerCount = Math.Clamp(Environment.ProcessorCount - 2, 1, 12);
        var workers = new Task[workerCount];
        for (int workerIndex = 0; workerIndex < workerCount; workerIndex++)
        {
            int capturedWorker = workerIndex;
            workers[workerIndex] = Task.Run(
                async () =>
                {
                    await using FileStream apk = OpenApk(apkPath);
                    using var archive = new ZipArchive(apk, ZipArchiveMode.Read, leaveOpen: false);
                    for (int index = capturedWorker; index < entries.Count; index += workerCount)
                    {
                        cancellationToken.ThrowIfCancellationRequested();
                        AssetEntry workItem = entries[index];
                        string entryName = workItem.Name;

                        try
                        {
                            ZipArchiveEntry? entry = archive.GetEntry(workItem.Name);
                            if (entry is null)
                            {
                                throw new InvalidDataException("ZIP 条目不存在。");
                            }

                            await using Stream input = entry.Open();
                            await using var output = new FileStream(
                                workItem.OutputPath,
                                new FileStreamOptions
                                {
                                    Mode = FileMode.Create,
                                    Access = FileAccess.Write,
                                    Share = FileShare.None,
                                    BufferSize = 256 * 1024,
                                    Options = FileOptions.Asynchronous | FileOptions.SequentialScan,
                                });
                            await input.CopyToAsync(output, 256 * 1024, cancellationToken).ConfigureAwait(false);
                            Interlocked.Increment(ref stats.AssetsExtracted);
                        }
                        catch (Exception exception) when (
                            exception is InvalidDataException or IOException &&
                            !cancellationToken.IsCancellationRequested)
                        {
                            TryDeleteFile(workItem.OutputPath);
                            Interlocked.Increment(ref stats.SkippedItems);
                            stats.Warnings.Enqueue($"跳过 APK 条目 {entryName}: {exception.Message}");
                        }

                        int done = Interlocked.Increment(ref completed);
                        if (done == entries.Count || done % 32 == 0)
                        {
                            double percent = 2 + 34d * done / entries.Count;
                            progress?.Report(new UnpackProgress(
                                UnpackStage.Extracting,
                                percent,
                                $"正在提取资源 {done:N0} / {entries.Count:N0}",
                                done,
                                entries.Count));
                        }
                    }
                },
                cancellationToken);
        }

        await Task.WhenAll(workers).ConfigureAwait(false);
    }

    private static async Task SplitAtlasesAsync(
        string[] plists,
        string stagingPath,
        string pngRoot,
        GameProfile profile,
        MutableUnpackStats stats,
        IProgress<UnpackProgress>? progress,
        CancellationToken cancellationToken)
    {
        if (plists.Length == 0)
        {
            return;
        }

        int completed = 0;
        var options = new ParallelOptions
        {
            CancellationToken = cancellationToken,
            MaxDegreeOfParallelism = Math.Clamp(Environment.ProcessorCount - 2, 1, 12),
        };

        await Parallel.ForEachAsync(
            plists,
            options,
            (plist, token) =>
            {
                try
                {
                    AtlasSplitResult result = AtlasSplitter.Split(
                        plist,
                        stagingPath,
                        pngRoot,
                        profile,
                        token,
                        () => Interlocked.Increment(ref stats.FramesWritten));
                    Interlocked.Increment(ref stats.AtlasesDecoded);
                    stats.KeyUsage.AddOrUpdate(result.TextureKey, 1, (_, count) => count + 1);
                    stats.KeyUsage.AddOrUpdate(result.PlistTransform, 1, (_, count) => count + 1);
                }
                catch (InvalidDataException exception) when (
                    exception.Message.Contains("不包含可拆分", StringComparison.Ordinal))
                {
                    Interlocked.Increment(ref stats.SkippedItems);
                }
                catch (Exception exception) when (
                    exception is InvalidDataException or IOException or NotSupportedException)
                {
                    Interlocked.Increment(ref stats.SkippedItems);
                    string relative = Path.GetRelativePath(stagingPath, plist);
                    stats.Warnings.Enqueue($"跳过 {relative}: {exception.Message}");
                }

                int done = Interlocked.Increment(ref completed);
                if (done == plists.Length || done % 8 == 0)
                {
                    double percent = 40 + 54d * done / plists.Length;
                    progress?.Report(new UnpackProgress(
                        UnpackStage.Splitting,
                        percent,
                        $"正在拆分图集 {done:N0} / {plists.Length:N0}，已输出 {Volatile.Read(ref stats.FramesWritten):N0} 张 PNG",
                        done,
                        plists.Length));
                }

                return ValueTask.CompletedTask;
            }).ConfigureAwait(false);
    }

    private static async Task WriteDocumentationAsync(
        string stagingPath,
        UnpackRequest request,
        MutableUnpackStats stats,
        TimeSpan elapsed,
        CancellationToken cancellationToken)
    {
        string pvrKeys = string.Join(
            Environment.NewLine,
            request.Profile.PvrKeys.Select(key =>
                $"- {key.Name}: `{FormatKey(key)}`"));
        string plistKey = request.Profile.PlistKey is null
            ? "- 本代 plist 为明文 XML。"
            : $"- {request.Profile.PlistKey.Name}: `{FormatKey(request.Profile.PlistKey)}`";

        string readme =
            $"""
             # {request.Profile.DisplayName}资源解包说明

             解包工具作者：熔萤FluorescentLava

             本工具的格式分析、算法整理、程序实现与验证借助了 AI 工具。

             ## 关键说明

             - 输入 APK：`{request.ApkPath}`
             - 游戏类型：{request.Profile.DisplayName}
             - `CCZ!` 为未加密压缩资源，`CCZp` 使用对应 key 解密后进行 zlib 解压。
             - 三代与阿波之旅的 `czzf` plist、四代的 `ff db ff ee 66` plist/PNG 会自动解密。
             - 自定义封装按“首尾各 512 个 32 位词连续、中段每 4 词处理 1 词”解密，并验证 header 中的长度与 CRC32。
             - RGBA4444 等 PVR v2 图像按 header 的通道 mask 解码，避免通道顺序造成偏色。
             - 拆分图位于 `Unpacked_PNG`；程序不输出整张 atlas PNG，也不会保留运行暂存目录。

             ## key

             {pvrKeys}
             {plistKey}

             ## 流程简写

             从 APK 并行提取 `assets`，在内存中解密 CCZ/PVR、plist 和加密 PNG，再按 plist 坐标并行拆出 PNG。其他流程略写。

             ## 本次统计

             - 提取资源：{stats.AssetsExtracted:N0}
             - 扫描 plist：{stats.PlistsScanned:N0}
             - 解出图集：{stats.AtlasesDecoded:N0}
             - 拆分 PNG：{stats.FramesWritten:N0}
             - 跳过项目：{stats.SkippedItems:N0}
             - 用时：{elapsed:hh\:mm\:ss}

             详细警告和 key 使用情况见 `_unpack_log.txt`。
             """;

        var log = new StringBuilder();
        log.AppendLine($"game={request.Profile.Kind}");
        log.AppendLine($"title={request.Profile.DisplayName}");
        log.AppendLine($"apk={request.ApkPath}");
        log.AppendLine($"assets_extracted={stats.AssetsExtracted}");
        log.AppendLine($"plist_scanned={stats.PlistsScanned}");
        log.AppendLine($"atlas_written={stats.AtlasesDecoded}");
        log.AppendLine($"frames_written={stats.FramesWritten}");
        log.AppendLine($"skipped={stats.SkippedItems}");
        log.AppendLine($"elapsed={elapsed}");
        log.AppendLine();
        log.AppendLine("[key_usage]");
        foreach ((string key, int count) in stats.KeyUsage.OrderBy(pair => pair.Key, StringComparer.Ordinal))
        {
            log.AppendLine($"{key}={count}");
        }

        log.AppendLine();
        log.AppendLine("[warnings]");
        foreach (string warning in stats.Warnings)
        {
            log.AppendLine(warning);
        }

        await File.WriteAllTextAsync(
            Path.Combine(stagingPath, "README.md"),
            readme,
            new UTF8Encoding(encoderShouldEmitUTF8Identifier: false),
            cancellationToken).ConfigureAwait(false);
        await File.WriteAllTextAsync(
            Path.Combine(stagingPath, "_unpack_log.txt"),
            log.ToString(),
            new UTF8Encoding(encoderShouldEmitUTF8Identifier: false),
            cancellationToken).ConfigureAwait(false);
    }

    private static void ReplaceOutputDirectory(string stagingPath, string finalPath, bool overwrite)
    {
        string? outputParent = Path.GetDirectoryName(finalPath);
        if (outputParent is null)
        {
            throw new InvalidOperationException("输出路径无效。");
        }

        if (!Directory.Exists(finalPath))
        {
            Directory.Move(stagingPath, finalPath);
            return;
        }

        if (!overwrite)
        {
            throw new IOException($"输出目录已存在：{finalPath}");
        }

        string backupPath = Path.Combine(
            outputParent,
            $".{Path.GetFileName(finalPath)}.backup-{Guid.NewGuid():N}");
        Directory.Move(finalPath, backupPath);
        try
        {
            Directory.Move(stagingPath, finalPath);
            TryDeleteOwnedDirectory(backupPath, outputParent);
        }
        catch
        {
            if (!Directory.Exists(finalPath) && Directory.Exists(backupPath))
            {
                Directory.Move(backupPath, finalPath);
            }

            throw;
        }
    }

    private static FileStream OpenApk(string path) =>
        new(
            path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.Read,
            256 * 1024,
            FileOptions.Asynchronous | FileOptions.RandomAccess);

    private static string ResolveZipOutputPath(string stagingPath, string relativeName)
    {
        string normalized = relativeName
            .Replace('/', Path.DirectorySeparatorChar)
            .Replace('\\', Path.DirectorySeparatorChar);
        string fullPath = Path.GetFullPath(Path.Combine(stagingPath, normalized));
        string safeRoot = Path.GetFullPath(stagingPath) + Path.DirectorySeparatorChar;
        if (!fullPath.StartsWith(safeRoot, StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidDataException($"APK 条目路径越界：{relativeName}");
        }

        return fullPath;
    }

    private static void ValidateRequest(UnpackRequest request)
    {
        if (!File.Exists(request.ApkPath))
        {
            throw new FileNotFoundException("找不到 APK 文件。", request.ApkPath);
        }

        if (!string.Equals(Path.GetExtension(request.ApkPath), ".apk", StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidDataException("只能选择 .apk 文件。");
        }

        if (string.IsNullOrWhiteSpace(request.OutputParent))
        {
            throw new InvalidDataException("请选择输出文件夹。");
        }

        using FileStream stream = File.OpenRead(request.ApkPath);
        Span<byte> signature = stackalloc byte[4];
        if (stream.Read(signature) != 4 ||
            signature[0] != (byte)'P' ||
            signature[1] != (byte)'K')
        {
            throw new InvalidDataException("文件不是有效的 APK/ZIP。");
        }
    }

    private static string FormatKey(EncryptionKey key) =>
        $"0x{key.Part0:X8}, 0x{key.Part1:X8}, 0x{key.Part2:X8}, 0x{key.Part3:X8}";

    private static void TryDeleteOwnedDirectory(string path, string ownerRoot)
    {
        try
        {
            string fullPath = Path.GetFullPath(path);
            string safeRoot = Path.GetFullPath(ownerRoot) + Path.DirectorySeparatorChar;
            if (fullPath.StartsWith(safeRoot, StringComparison.OrdinalIgnoreCase) && Directory.Exists(fullPath))
            {
                Directory.Delete(fullPath, recursive: true);
            }
        }
        catch (IOException)
        {
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    private static void TryDeleteFile(string path)
    {
        try
        {
            if (File.Exists(path))
            {
                File.Delete(path);
            }
        }
        catch (IOException)
        {
        }
    }
}
