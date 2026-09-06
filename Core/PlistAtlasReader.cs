using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using System.Xml;
using System.Xml.Linq;
using CFUnpacker.Models;

namespace CFUnpacker.Core;

internal readonly record struct IntSize(int Width, int Height);
internal readonly record struct IntRect(int X, int Y, int Width, int Height);

internal sealed record AtlasFrame(
    string Name,
    IntRect Frame,
    IntSize SourceSize,
    IntRect SourceColorRect,
    bool Rotated);

internal sealed record AtlasDefinition(
    string? TextureFileName,
    IReadOnlyList<AtlasFrame> Frames,
    string PlistTransform);

internal static partial class PlistAtlasReader
{
    public static AtlasDefinition? Read(string plistPath, GameProfile profile)
    {
        byte[] source = File.ReadAllBytes(plistPath);
        (byte[] xml, string transform) = DecodeXml(source, profile);
        Dictionary<string, object?> root = ParseRoot(xml);

        if (!TryGetDictionary(root, "frames", out Dictionary<string, object?>? framesDictionary) ||
            framesDictionary.Count == 0)
        {
            return null;
        }

        var frames = new List<AtlasFrame>(framesDictionary.Count);
        foreach ((string frameName, object? rawFrame) in framesDictionary)
        {
            if (rawFrame is not Dictionary<string, object?> values ||
                !TryReadRect(values, ["frame", "textureRect"], out IntRect frame))
            {
                continue;
            }

            bool rotated = GetBoolean(values, "rotated") || GetBoolean(values, "textureRotated");
            IntSize packedSize = TryReadSize(values, ["spriteSize"], out IntSize spriteSize)
                ? spriteSize
                : new IntSize(frame.Width, frame.Height);

            IntSize sourceSize = TryReadSize(values, ["sourceSize", "spriteSourceSize"], out IntSize size)
                ? size
                : packedSize;

            IntRect sourceColorRect;
            if (!TryReadRect(values, ["sourceColorRect", "spriteSourceSize"], out sourceColorRect))
            {
                (int offsetX, int offsetY) = ReadPoint(values, ["offset", "spriteOffset"]);
                int destinationX = (sourceSize.Width - packedSize.Width) / 2 + offsetX;
                int destinationY = (sourceSize.Height - packedSize.Height) / 2 - offsetY;
                sourceColorRect = new IntRect(
                    destinationX,
                    destinationY,
                    packedSize.Width,
                    packedSize.Height);
            }

            frames.Add(new AtlasFrame(frameName, frame, sourceSize, sourceColorRect, rotated));
        }

        if (frames.Count == 0)
        {
            return null;
        }

        string? textureFileName = null;
        if (TryGetDictionary(root, "metadata", out Dictionary<string, object?>? metadata))
        {
            textureFileName = GetString(metadata, "realTextureFileName") ??
                              GetString(metadata, "textureFileName");
        }

        textureFileName ??= GetString(root, "textureFileName");
        return new AtlasDefinition(textureFileName, frames, transform);
    }

    private static (byte[] Xml, string Transform) DecodeXml(byte[] data, GameProfile profile)
    {
        if (LooksLikeXml(data))
        {
            return (StripMergeConflictMarkers(data), "明文 plist");
        }

        bool isCzzf = data.AsSpan().StartsWith("czzf"u8);
        bool isCarrot4 = data.AsSpan().StartsWith(new byte[] { 0xFF, 0xDB, 0xFF, 0xEE, 0x66 });
        if ((isCzzf || isCarrot4) &&
            profile.PlistKey is not null &&
            EncryptionCodec.TryDecodeCustom(
                data,
                profile.PlistKey,
                profile.PlistKeyRounds,
                out byte[] decoded,
                out string transform))
        {
            return (StripMergeConflictMarkers(decoded), $"{profile.PlistKey.Name} ({transform})");
        }

        throw new InvalidDataException("plist 解密失败，所选游戏与 APK 可能不匹配。");
    }

    /// <summary>
    /// 保卫萝卜4 1.0.0 等版本的开发者把带 Git 合并冲突标记的 plist 直接打进了包里
    /// （如 dollhouse_abo_huawei_clothes.plist 的 smartupdate 值），严格 XML 解析会失败。
    /// 这里整行剥掉冲突标记：保留的两个版本值中，宽容的字典解析会自动跳过多余的一个。
    /// </summary>
    private static byte[] StripMergeConflictMarkers(byte[] data)
    {
        if (data.AsSpan().IndexOf("<<<<<<<"u8) < 0)
        {
            return data;
        }

        var builder = new StringBuilder(data.Length);
        using var reader = new StringReader(Encoding.UTF8.GetString(data));
        while (reader.ReadLine() is { } line)
        {
            string trimmed = line.TrimStart();
            bool isMarker =
                trimmed.StartsWith("<<<<<<<", StringComparison.Ordinal) ||
                trimmed.StartsWith(">>>>>>>", StringComparison.Ordinal) ||
                trimmed == "=======";
            if (!isMarker)
            {
                builder.Append(line).Append('\n');
            }
        }

        return Encoding.UTF8.GetBytes(builder.ToString());
    }

    private static Dictionary<string, object?> ParseRoot(byte[] xml)
    {
        var settings = new XmlReaderSettings
        {
            DtdProcessing = DtdProcessing.Ignore,
            XmlResolver = null,
            IgnoreComments = true,
            IgnoreWhitespace = true,
        };
        using var stream = new MemoryStream(xml, writable: false);
        using XmlReader reader = XmlReader.Create(stream, settings);
        XDocument document = XDocument.Load(reader, LoadOptions.None);
        XElement? dictionary = document.Root?.Elements().FirstOrDefault(element => element.Name.LocalName == "dict");
        return dictionary is null
            ? throw new InvalidDataException("plist 中没有根 dict。")
            : ParseDictionary(dictionary);
    }

    private static Dictionary<string, object?> ParseDictionary(XElement dictionary)
    {
        var result = new Dictionary<string, object?>(StringComparer.Ordinal);
        using IEnumerator<XElement> elements = dictionary.Elements().GetEnumerator();
        while (elements.MoveNext())
        {
            XElement keyElement = elements.Current;
            if (keyElement.Name.LocalName != "key" || !elements.MoveNext())
            {
                continue;
            }

            result[keyElement.Value] = ParseValue(elements.Current);
        }

        return result;
    }

    private static object? ParseValue(XElement element) =>
        element.Name.LocalName switch
        {
            "dict" => ParseDictionary(element),
            "array" => element.Elements().Select(ParseValue).ToList(),
            "string" or "date" or "data" => element.Value,
            "integer" => long.TryParse(element.Value, NumberStyles.Integer, CultureInfo.InvariantCulture, out long integer)
                ? integer
                : element.Value,
            "real" => double.TryParse(element.Value, NumberStyles.Float, CultureInfo.InvariantCulture, out double real)
                ? real
                : element.Value,
            "true" => true,
            "false" => false,
            _ => element.Value,
        };

    private static bool TryGetDictionary(
        IReadOnlyDictionary<string, object?> dictionary,
        string key,
        out Dictionary<string, object?> value)
    {
        if (dictionary.TryGetValue(key, out object? raw) && raw is Dictionary<string, object?> typed)
        {
            value = typed;
            return true;
        }

        value = null!;
        return false;
    }

    private static bool TryReadRect(
        IReadOnlyDictionary<string, object?> values,
        IReadOnlyList<string> keys,
        out IntRect rect)
    {
        foreach (string key in keys)
        {
            if (!values.TryGetValue(key, out object? raw))
            {
                continue;
            }

            if (raw is string text)
            {
                int[] numbers = ParseNumbers(text);
                if (numbers.Length >= 4)
                {
                    rect = new IntRect(numbers[0], numbers[1], numbers[2], numbers[3]);
                    return true;
                }
            }
            else if (raw is Dictionary<string, object?> dictionary &&
                     TryGetInteger(dictionary, "x", out int x) &&
                     TryGetInteger(dictionary, "y", out int y) &&
                     TryGetInteger(dictionary, "width", out int width) &&
                     TryGetInteger(dictionary, "height", out int height))
            {
                rect = new IntRect(x, y, width, height);
                return true;
            }
        }

        rect = default;
        return false;
    }

    private static bool TryReadSize(
        IReadOnlyDictionary<string, object?> values,
        IReadOnlyList<string> keys,
        out IntSize size)
    {
        foreach (string key in keys)
        {
            if (!values.TryGetValue(key, out object? raw))
            {
                continue;
            }

            if (raw is string text)
            {
                int[] numbers = ParseNumbers(text);
                if (numbers.Length >= 2)
                {
                    int start = numbers.Length >= 4 ? numbers.Length - 2 : 0;
                    size = new IntSize(numbers[start], numbers[start + 1]);
                    return true;
                }
            }
            else if (raw is Dictionary<string, object?> dictionary &&
                     TryGetInteger(dictionary, "width", out int width) &&
                     TryGetInteger(dictionary, "height", out int height))
            {
                size = new IntSize(width, height);
                return true;
            }
        }

        size = default;
        return false;
    }

    private static (int X, int Y) ReadPoint(
        IReadOnlyDictionary<string, object?> values,
        IReadOnlyList<string> keys)
    {
        foreach (string key in keys)
        {
            string? text = GetString(values, key);
            int[] numbers = text is null ? [] : ParseNumbers(text);
            if (numbers.Length >= 2)
            {
                return (numbers[0], numbers[1]);
            }
        }

        return (0, 0);
    }

    private static bool GetBoolean(IReadOnlyDictionary<string, object?> values, string key) =>
        values.TryGetValue(key, out object? value) &&
        (value is true || value is string text && bool.TryParse(text, out bool parsed) && parsed);

    private static string? GetString(IReadOnlyDictionary<string, object?> values, string key) =>
        values.TryGetValue(key, out object? value) ? value as string : null;

    private static bool TryGetInteger(
        IReadOnlyDictionary<string, object?> values,
        string key,
        out int result)
    {
        if (values.TryGetValue(key, out object? value))
        {
            if (value is long integer && integer is >= int.MinValue and <= int.MaxValue)
            {
                result = (int)integer;
                return true;
            }

            if (value is string text && int.TryParse(text, out result))
            {
                return true;
            }
        }

        result = 0;
        return false;
    }

    private static int[] ParseNumbers(string value) =>
        SignedIntegerRegex()
            .Matches(value)
            .Select(match => int.Parse(match.Value, CultureInfo.InvariantCulture))
            .ToArray();

    private static bool LooksLikeXml(ReadOnlySpan<byte> data)
    {
        int index = 0;
        if (data.StartsWith(new byte[] { 0xEF, 0xBB, 0xBF }))
        {
            index = 3;
        }

        while (index < data.Length && char.IsWhiteSpace((char)data[index]))
        {
            index++;
        }

        return index < data.Length && data[index] == (byte)'<';
    }

    [GeneratedRegex(@"-?\d+", RegexOptions.CultureInvariant)]
    private static partial Regex SignedIntegerRegex();
}
