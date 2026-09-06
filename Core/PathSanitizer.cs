using System.Globalization;
using System.Text;

namespace CFUnpacker.Core;

/// <summary>
/// 把 ZIP/plist 里的资源名映射为 Windows 可写的文件名。
/// APK（如保卫萝卜4 1.0.6）里存在含 &lt;、&gt; 等字符的条目名，直接写盘会失败；
/// 非法字符按百分号编码（&lt; → %3C）替换，保留可读性。
/// plist 引用纹理时也必须经过同一规则消毒，两侧才能对得上。
/// </summary>
internal static class PathSanitizer
{
    private static readonly HashSet<string> ReservedNames = new(StringComparer.OrdinalIgnoreCase)
    {
        "CON", "PRN", "AUX", "NUL",
        "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    };

    public static string SanitizeSegment(string segment)
    {
        char[] invalid = Path.GetInvalidFileNameChars();
        var builder = new StringBuilder(segment.Length);
        foreach (char character in segment)
        {
            if (invalid.Contains(character) || char.IsControl(character))
            {
                builder.Append(PercentEncode(character));
            }
            else
            {
                builder.Append(character);
            }
        }

        string sanitized = builder.ToString();
        if (string.IsNullOrWhiteSpace(sanitized))
        {
            return "_";
        }

        // 结尾的点/空格会被 Windows 剥掉导致找不到文件，同样编码保留。
        if (sanitized.EndsWith('.') || sanitized.EndsWith(' '))
        {
            sanitized = sanitized[..^1] + PercentEncode(sanitized[^1]);
        }

        string nameWithoutExtension = Path.GetFileNameWithoutExtension(sanitized);
        if (nameWithoutExtension.Length > 0 && ReservedNames.Contains(nameWithoutExtension))
        {
            sanitized = "_" + sanitized;
        }

        return sanitized;
    }

    public static string SanitizeRelativePath(string relativePath)
    {
        string normalized = relativePath.Replace('/', Path.DirectorySeparatorChar);
        string[] segments = normalized
            .Split(Path.DirectorySeparatorChar, StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
            .Where(segment => segment is not "." and not "..")
            .Select(SanitizeSegment)
            .ToArray();
        return segments.Length == 0 ? "_" : string.Join(Path.DirectorySeparatorChar, segments);
    }

    private static string PercentEncode(char value)
    {
        var builder = new StringBuilder(3);
        foreach (byte byteValue in Encoding.UTF8.GetBytes(value.ToString()))
        {
            builder.Append('%').Append(byteValue.ToString("X2", CultureInfo.InvariantCulture));
        }

        return builder.ToString();
    }
}
