using System.Buffers.Binary;
using System.IO.Compression;
using System.Text;

namespace CFUnpacker.Core;

/// <summary>分包里的一个内层 APK：要么是已解出的临时文件，要么是外层文件里的零拷贝窗口。</summary>
internal sealed record BundleApk(string Name, string? ExtractedPath, long DataOffset, long DataLength)
{
    public bool IsWindow => ExtractedPath is null;
}

/// <summary>
/// 处理 .apks / .xapk 分包格式。外层是一个 zip，内含 base.apk 与若干 split APK。
/// bundletool / Play 生成的分包内层 APK 以 STORED 方式存放，数据在外层文件中连续，
/// 直接用窗口流（<see cref="SubStream"/>）作为 ZipArchive 输入，免去几百 MB 的磁盘拷贝；
/// 仅当内层被压缩时才回退为解到临时目录。
/// </summary>
internal sealed class ApkBundle : IDisposable
{
    private static readonly string[] BundleExtensions = [".apks", ".xapk"];
    private readonly string? _tempDirectory;

    private ApkBundle(string bundlePath, List<BundleApk> apks, string? tempDirectory)
    {
        BundlePath = bundlePath;
        Apks = apks;
        _tempDirectory = tempDirectory;
    }

    public string BundlePath { get; }

    public IReadOnlyList<BundleApk> Apks { get; }

    public static bool IsBundle(string path) =>
        BundleExtensions.Contains(Path.GetExtension(TrimNumericSuffix(path)), StringComparer.OrdinalIgnoreCase);

    public static bool IsSupportedInput(string path)
    {
        string extension = Path.GetExtension(TrimNumericSuffix(path));
        return string.Equals(extension, ".apk", StringComparison.OrdinalIgnoreCase) || IsBundle(path);
    }

    /// <summary>
    /// QQ 下载会把 .apk 改名成 .apk.1（重复下载递增为 .2、.3……）。
    /// 末尾是纯数字扩展名时剥掉，露出真实扩展名。
    /// </summary>
    public static string TrimNumericSuffix(string path)
    {
        ReadOnlySpan<char> extension = Path.GetExtension(path.AsSpan());
        if (extension.Length > 1)
        {
            bool digitsOnly = true;
            foreach (char character in extension[1..])
            {
                if (!char.IsDigit(character))
                {
                    digitsOnly = false;
                    break;
                }
            }

            if (digitsOnly)
            {
                return path[..^extension.Length];
            }
        }

        return path;
    }

    /// <summary>普通 .apk 返回自身；分包解析出 base 在前、跳过 split_config.* 的 APK 列表。</summary>
    public static ApkBundle Resolve(string inputPath)
    {
        if (!IsBundle(inputPath))
        {
            return new ApkBundle(inputPath, [new BundleApk(Path.GetFileName(inputPath), inputPath, 0, 0)], null);
        }

        List<(string Name, int Method, long LocalHeaderOffset, long Size)> entries = ReadOuterEntries(inputPath);
        var apks = new List<BundleApk>();
        string? tempDirectory = null;
        foreach ((string name, int method, long localHeaderOffset, long size) in
                 entries.Where(entry => IsInnerApk(entry.Name))
                     .OrderBy(entry => string.Equals(entry.Name, "base.apk", StringComparison.OrdinalIgnoreCase) ? 0 : 1)
                     .ThenBy(entry => entry.Name, StringComparer.Ordinal))
        {
            BundleApk? apk = null;
            if (method == 0 && size > 0)
            {
                apk = TryCreateWindow(inputPath, name, localHeaderOffset, size);
            }

            if (apk is null)
            {
                tempDirectory ??= Path.Combine(
                    Path.GetTempPath(),
                    $".cfunpacker-bundle-{Guid.NewGuid():N}");
                Directory.CreateDirectory(tempDirectory);
                apk = ExtractToTemp(inputPath, name, tempDirectory);
            }

            apks.Add(apk);
        }

        if (apks.Count == 0)
        {
            throw new InvalidDataException("分包中没有找到任何 APK。");
        }

        return new ApkBundle(inputPath, apks, tempDirectory);
    }

    /// <summary>打开一组独立的归档实例（每个 worker 各调一次，避免共享句柄）。</summary>
    public List<(Stream OwnerStream, ZipArchive Archive)> OpenArchives()
    {
        var opened = new List<(Stream, ZipArchive)>(Apks.Count);
        foreach (BundleApk apk in Apks)
        {
            if (apk.ExtractedPath is not null)
            {
                FileStream stream = OpenPlain(apk.ExtractedPath);
                try
                {
                    opened.Add((stream, new ZipArchive(stream, ZipArchiveMode.Read, leaveOpen: false)));
                }
                catch
                {
                    stream.Dispose();
                    throw;
                }
            }
            else
            {
                // ZipArchive 只负责释放窗口流；底层 FileStream 由调用方经 OwnerStream 释放。
                FileStream file = OpenPlain(BundlePath);
                try
                {
                    var window = new SubStream(file, apk.DataOffset, apk.DataLength);
                    opened.Add((file, new ZipArchive(window, ZipArchiveMode.Read, leaveOpen: false)));
                }
                catch
                {
                    file.Dispose();
                    throw;
                }
            }
        }

        return opened;
    }

    public void Dispose()
    {
        if (_tempDirectory is null)
        {
            return;
        }

        try
        {
            if (Directory.Exists(_tempDirectory))
            {
                Directory.Delete(_tempDirectory, recursive: true);
            }
        }
        catch (IOException)
        {
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    private static bool IsInnerApk(string entryName)
    {
        string name = entryName.Replace('\\', '/');
        return name.EndsWith(".apk", StringComparison.OrdinalIgnoreCase) &&
               !Path.GetFileName(name).StartsWith("split_config.", StringComparison.OrdinalIgnoreCase);
    }

    private static FileStream OpenPlain(string path) =>
        new(
            path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.Read,
            64 * 1024,
            FileOptions.RandomAccess);

    /// <summary>解析外层 zip 的 EOCD 与中央目录，取每个条目的存储方式与本地头偏移。</summary>
    private static List<(string Name, int Method, long LocalHeaderOffset, long Size)> ReadOuterEntries(string path)
    {
        using var file = new FileStream(
            path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.Read,
            64 * 1024,
            FileOptions.RandomAccess);
        long centralDirectoryOffset = LocateCentralDirectory(file, out int entryCount);
        if (centralDirectoryOffset < 0 || entryCount == 0)
        {
            throw new InvalidDataException("分包中央目录无法解析。");
        }

        var result = new List<(string, int, long, long)>(entryCount);
        byte[] record = new byte[46];
        long offset = centralDirectoryOffset;
        for (int index = 0; index < entryCount; index++)
        {
            ReadExact(file, offset, record);
            if (record[0] != (byte)'P' || record[1] != (byte)'K' || record[2] != 1 || record[3] != 2)
            {
                throw new InvalidDataException("分包中央目录记录损坏。");
            }

            int method = BinaryPrimitives.ReadUInt16LittleEndian(record.AsSpan(10));
            uint compressedSize = BinaryPrimitives.ReadUInt32LittleEndian(record.AsSpan(20));
            uint uncompressedSize = BinaryPrimitives.ReadUInt32LittleEndian(record.AsSpan(24));
            int nameLength = BinaryPrimitives.ReadUInt16LittleEndian(record.AsSpan(28));
            int extraLength = BinaryPrimitives.ReadUInt16LittleEndian(record.AsSpan(30));
            int commentLength = BinaryPrimitives.ReadUInt16LittleEndian(record.AsSpan(32));
            uint localHeaderOffset = BinaryPrimitives.ReadUInt32LittleEndian(record.AsSpan(42));
            if (localHeaderOffset == 0xFFFFFFFF || uncompressedSize == 0xFFFFFFFF)
            {
                // ZIP64：直接走临时文件回退。
                result.Add((Encoding.UTF8.GetString(ReadRange(file, offset + 46, nameLength)), -1, -1, -1));
            }
            else
            {
                result.Add((
                    Encoding.UTF8.GetString(ReadRange(file, offset + 46, nameLength)),
                    method,
                    localHeaderOffset,
                    uncompressedSize));
            }

            offset += 46 + nameLength + extraLength + commentLength;
        }

        return result;
    }

    private static long LocateCentralDirectory(FileStream file, out int entryCount)
    {
        long scanStart = Math.Max(0, file.Length - 66000);
        int tailLength = (int)(file.Length - scanStart);
        byte[] tail = new byte[tailLength];
        file.Position = scanStart;
        int read = 0;
        while (read < tailLength)
        {
            int chunk = file.Read(tail, read, tailLength - read);
            if (chunk == 0)
            {
                break;
            }

            read += chunk;
        }

        // 从后往前找 EOCD 签名 PK\x05\x06（固定 22 字节，之后最多 64KB 注释）。
        for (int index = read - 22; index >= 0; index--)
        {
            if (tail[index] == (byte)'P' &&
                tail[index + 1] == (byte)'K' &&
                tail[index + 2] == 5 &&
                tail[index + 3] == 6)
            {
                entryCount = BinaryPrimitives.ReadUInt16LittleEndian(tail.AsSpan(index + 10));
                uint directoryOffset = BinaryPrimitives.ReadUInt32LittleEndian(tail.AsSpan(index + 16));
                if (directoryOffset == 0xFFFFFFFF)
                {
                    entryCount = 0;
                    return -1;
                }

                return directoryOffset;
            }
        }

        entryCount = 0;
        return -1;
    }

    private static BundleApk? TryCreateWindow(string path, string name, long localHeaderOffset, long size)
    {
        try
        {
            using var file = OpenPlain(path);
            byte[] localHeader = ReadRange(file, localHeaderOffset, 30);
            if (localHeader[0] != (byte)'P' || localHeader[1] != (byte)'K' || localHeader[2] != 3 || localHeader[3] != 4)
            {
                return null;
            }

            int nameLength = BinaryPrimitives.ReadUInt16LittleEndian(localHeader.AsSpan(26));
            int extraLength = BinaryPrimitives.ReadUInt16LittleEndian(localHeader.AsSpan(28));
            long dataOffset = localHeaderOffset + 30 + nameLength + extraLength;

            // 用 ZipArchive 实际解析一次中央目录来验证窗口正确性。
            using var window = new SubStream(file, dataOffset, size);
            using var archive = new ZipArchive(window, ZipArchiveMode.Read, leaveOpen: false);
            if (archive.Entries.Count == 0)
            {
                return null;
            }

            return new BundleApk(name, null, dataOffset, size);
        }
        catch (InvalidDataException)
        {
            return null;
        }
        catch (IOException)
        {
            return null;
        }
        catch (ArgumentOutOfRangeException)
        {
            return null;
        }
    }

    private static BundleApk ExtractToTemp(string path, string name, string tempDirectory)
    {
        using var archive = ZipFile.OpenRead(path);
        ZipArchiveEntry? entry = archive.GetEntry(name);
        if (entry is null)
        {
            throw new InvalidDataException($"分包条目不存在：{name}");
        }

        string targetPath = Path.Combine(tempDirectory, name.Replace('/', '_'));
        entry.ExtractToFile(targetPath, overwrite: true);
        return new BundleApk(name, targetPath, 0, 0);
    }

    private static byte[] ReadRange(FileStream file, long offset, int length)
    {
        var buffer = new byte[length];
        file.Position = offset;
        int read = 0;
        while (read < length)
        {
            int chunk = file.Read(buffer, read, length - read);
            if (chunk == 0)
            {
                throw new InvalidDataException("分包数据不完整。");
            }

            read += chunk;
        }

        return buffer;
    }

    private static void ReadExact(FileStream file, long offset, byte[] buffer)
    {
        file.Position = offset;
        int read = 0;
        while (read < buffer.Length)
        {
            int chunk = file.Read(buffer, read, buffer.Length - read);
            if (chunk == 0)
            {
                throw new InvalidDataException("分包数据不完整。");
            }

            read += chunk;
        }
    }
}
