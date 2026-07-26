using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.IO.Compression;
using CarrotUnpacker.Models;

namespace CarrotUnpacker.Core;

internal static class EncryptionCodec
{
    private const int KeyStreamLength = 1024;
    private const int CczSecureWordCount = 512;
    private const int CczSparseDistance = 64;
    private const int WrappedEdgeWordCount = 512;
    private const int WrappedMiddleDistance = 4;
    private static readonly ConcurrentDictionary<string, uint[]> KeyStreamCache = new();
    private static readonly uint[] CrcTable = CreateCrcTable();

    public static bool IsCcz(ReadOnlySpan<byte> data) =>
        data.Length >= 16 &&
        data[0] == (byte)'C' &&
        data[1] == (byte)'C' &&
        data[2] == (byte)'Z' &&
        (data[3] == (byte)'!' || data[3] == (byte)'p');

    public static byte[] DecodeCcz(
        ReadOnlySpan<byte> data,
        IReadOnlyList<EncryptionKey> keys,
        out string keyName)
    {
        if (!IsCcz(data))
        {
            throw new InvalidDataException("不是 CCZ 数据。");
        }

        if (data[3] == (byte)'!')
        {
            keyName = "明文 CCZ";
            return InflateCcz(data);
        }

        foreach (EncryptionKey key in keys)
        {
            byte[] candidate = data.ToArray();
            TransformCczInPlace(candidate.AsSpan(12), key, rounds: 6);

            try
            {
                byte[] decoded = InflateCcz(candidate);
                if (LooksLikePvr(decoded))
                {
                    keyName = key.Name;
                    return decoded;
                }
            }
            catch (InvalidDataException)
            {
                // Try the compatibility keys supplied by the selected profile.
            }
        }

        throw new InvalidDataException("CCZ 解密失败，所选游戏与 APK 可能不匹配。");
    }

    public static bool TryDecodeCustom(
        ReadOnlySpan<byte> data,
        EncryptionKey key,
        int rounds,
        out byte[] decoded,
        out string transform)
    {
        decoded = [];
        transform = string.Empty;
        if (data.Length < 24)
        {
            return false;
        }

        int expectedLength;
        try
        {
            expectedLength = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(16, 4)));
        }
        catch (OverflowException)
        {
            return false;
        }

        if (expectedLength <= 0 || expectedLength > 1024 * 1024 * 1024)
        {
            return false;
        }

        uint expectedCrc = BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(8, 4));
        byte[] encryptedPayload = data[20..].ToArray();
        TransformWrappedPayloadInPlace(encryptedPayload, key, rounds);

        try
        {
            byte[] candidate = Inflate(encryptedPayload, expectedLength);
            if (candidate.Length != expectedLength ||
                (expectedCrc != 0 && ComputeCrc32(candidate) != expectedCrc) ||
                (!LooksLikeXml(candidate) && !LooksLikePng(candidate) && !LooksLikePvr(candidate)))
            {
                return false;
            }

            decoded = candidate;
            transform =
                $"首尾各 {WrappedEdgeWordCount} 词连续、中段每 {WrappedMiddleDistance} 词，" +
                $"{rounds} 轮，CRC32 已验证";
            return true;
        }
        catch (InvalidDataException)
        {
            return false;
        }
    }

    private static byte[] InflateCcz(ReadOnlySpan<byte> data)
    {
        uint rawExpectedLength = BinaryPrimitives.ReadUInt32BigEndian(data.Slice(12, 4));
        if (rawExpectedLength == 0 || rawExpectedLength > 1024U * 1024U * 1024U)
        {
            throw new InvalidDataException("CCZ 解压长度无效。");
        }

        int expectedLength = (int)rawExpectedLength;
        byte[] output = Inflate(data[16..], expectedLength);
        if (output.Length != expectedLength)
        {
            throw new InvalidDataException("CCZ 解压长度不一致。");
        }

        return output;
    }

    private static byte[] Inflate(ReadOnlySpan<byte> compressed, int expectedLength)
    {
        using var source = new MemoryStream(compressed.ToArray(), writable: false);
        using var zlib = new ZLibStream(source, CompressionMode.Decompress);
        using var destination = new MemoryStream(expectedLength);
        try
        {
            zlib.CopyTo(destination);
            return destination.ToArray();
        }
        catch (IOException exception)
        {
            throw new InvalidDataException("zlib 数据无效。", exception);
        }
    }

    private static void TransformCczInPlace(
        Span<byte> data,
        EncryptionKey key,
        int rounds)
    {
        uint[] keyStream = GetKeyStream(key, rounds);
        int wordCount = data.Length / sizeof(uint);
        int keyIndex = 0;
        int wordIndex = 0;

        for (; wordIndex < wordCount && wordIndex < CczSecureWordCount; wordIndex++)
        {
            XorWord(data, wordIndex, keyStream[keyIndex++ % KeyStreamLength]);
        }

        for (; wordIndex < wordCount; wordIndex += CczSparseDistance)
        {
            XorWord(data, wordIndex, keyStream[keyIndex++ % KeyStreamLength]);
        }
    }

    private static void TransformWrappedPayloadInPlace(
        Span<byte> data,
        EncryptionKey key,
        int rounds)
    {
        uint[] keyStream = GetKeyStream(key, rounds);
        int wordCount = data.Length / sizeof(uint);
        int keyIndex = 0;
        int wordIndex = 0;

        for (; wordIndex < wordCount && wordIndex < WrappedEdgeWordCount; wordIndex++)
        {
            XorWord(data, wordIndex, keyStream[keyIndex++ % KeyStreamLength]);
        }

        int middleLimit = wordCount - WrappedEdgeWordCount;
        for (; wordIndex < wordCount && wordIndex < middleLimit; wordIndex += WrappedMiddleDistance)
        {
            XorWord(data, wordIndex, keyStream[keyIndex++ % KeyStreamLength]);
        }

        for (; wordIndex < wordCount; wordIndex++)
        {
            XorWord(data, wordIndex, keyStream[keyIndex++ % KeyStreamLength]);
        }
    }

    private static void XorWord(Span<byte> data, int wordIndex, uint key)
    {
        Span<byte> word = data.Slice(wordIndex * sizeof(uint), sizeof(uint));
        uint value = BinaryPrimitives.ReadUInt32LittleEndian(word);
        BinaryPrimitives.WriteUInt32LittleEndian(word, value ^ key);
    }

    private static uint[] GetKeyStream(EncryptionKey key, int rounds)
    {
        string cacheKey = $"{key.Part0:X8}:{key.Part1:X8}:{key.Part2:X8}:{key.Part3:X8}:{rounds}";
        return KeyStreamCache.GetOrAdd(cacheKey, _ => GenerateKeyStream(key.Parts, rounds));
    }

    private static uint[] GenerateKeyStream(uint[] parts, int rounds)
    {
        var stream = new uint[KeyStreamLength];
        uint z = stream[^1];
        uint sum = 0;

        unchecked
        {
            for (int round = 0; round < rounds; round++)
            {
                sum += 0x9E3779B9;
                uint e = (sum >> 2) & 3;

                uint p;
                uint y;
                for (p = 0; p < KeyStreamLength - 1; p++)
                {
                    y = stream[p + 1];
                    z = stream[p] += Mix(z, y, sum, parts[(p & 3) ^ e]);
                }

                y = stream[0];
                stream[^1] += Mix(z, y, sum, parts[(p & 3) ^ e]);
                z = stream[^1];
            }
        }

        return stream;
    }

    private static uint Mix(uint z, uint y, uint sum, uint keyPart) =>
        unchecked((((z >> 5) ^ (y << 2)) + ((y >> 3) ^ (z << 4))) ^
                  ((sum ^ y) + (keyPart ^ z)));

    private static bool LooksLikePvr(ReadOnlySpan<byte> data)
    {
        if (data.Length < 52)
        {
            return false;
        }

        return BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(44, 4)) == 0x21525650 ||
               BinaryPrimitives.ReadUInt32LittleEndian(data[..4]) == 0x03525650;
    }

    private static bool LooksLikeXml(ReadOnlySpan<byte> data)
    {
        int offset = data.StartsWith(new byte[] { 0xEF, 0xBB, 0xBF }) ? 3 : 0;
        while (offset < data.Length && data[offset] is (byte)' ' or (byte)'\r' or (byte)'\n' or (byte)'\t')
        {
            offset++;
        }

        return data.Length - offset >= 5 &&
               data[offset] == (byte)'<' &&
               (data[offset + 1] == (byte)'?' ||
                data.Slice(offset, Math.Min(6, data.Length - offset)).StartsWith("<plist"u8));
    }

    private static bool LooksLikePng(ReadOnlySpan<byte> data) =>
        data.StartsWith(new byte[] { 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A });

    private static uint ComputeCrc32(ReadOnlySpan<byte> data)
    {
        uint crc = uint.MaxValue;
        foreach (byte value in data)
        {
            crc = CrcTable[(crc ^ value) & 0xFF] ^ (crc >> 8);
        }

        return ~crc;
    }

    private static uint[] CreateCrcTable()
    {
        var table = new uint[256];
        for (uint i = 0; i < table.Length; i++)
        {
            uint value = i;
            for (int bit = 0; bit < 8; bit++)
            {
                value = (value & 1) != 0
                    ? 0xEDB88320U ^ (value >> 1)
                    : value >> 1;
            }

            table[i] = value;
        }

        return table;
    }
}
