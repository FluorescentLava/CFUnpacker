using System.Buffers.Binary;
using System.Numerics;
using System.Runtime.InteropServices;
using SkiaSharp;

namespace CarrotUnpacker.Core;

internal static class PvrDecoder
{
    private const uint Pvr2Tag = 0x21525650;
    private const uint Pvr3Tag = 0x03525650;

    public static SKBitmap Decode(ReadOnlySpan<byte> data, bool unpremultiplyAlpha = false)
    {
        if (data.Length >= 52 && BinaryPrimitives.ReadUInt32LittleEndian(data[..4]) == Pvr3Tag)
        {
            return DecodeV3(data, unpremultiplyAlpha);
        }

        if (data.Length >= 52 && BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(44, 4)) == Pvr2Tag)
        {
            return DecodeV2(data, unpremultiplyAlpha);
        }

        throw new InvalidDataException("不支持或已损坏的 PVR 文件。");
    }

    private static SKBitmap DecodeV2(ReadOnlySpan<byte> data, bool unpremultiplyAlpha)
    {
        int headerLength = checked((int)ReadUInt32(data, 0));
        int height = checked((int)ReadUInt32(data, 4));
        int width = checked((int)ReadUInt32(data, 8));
        uint flags = ReadUInt32(data, 16);
        int bitsPerPixel = checked((int)ReadUInt32(data, 24));
        uint redMask = ReadUInt32(data, 28);
        uint greenMask = ReadUInt32(data, 32);
        uint blueMask = ReadUInt32(data, 36);
        uint alphaMask = ReadUInt32(data, 40);
        int pixelType = (int)(flags & 0xFF);

        ValidateDimensions(width, height);
        if (headerLength < 52 || headerLength > data.Length)
        {
            throw new InvalidDataException("PVR v2 header 长度无效。");
        }

        if (pixelType is 0x18 or 0x19)
        {
            throw new NotSupportedException("当前资源使用 PVRTC 压缩，尚不能直接拆分。");
        }

        int bytesPerPixel = bitsPerPixel switch
        {
            8 => 1,
            16 => 2,
            24 => 3,
            32 => 4,
            _ => throw new NotSupportedException($"不支持的 PVR v2 位深：{bitsPerPixel}。"),
        };

        int pixelBytes = checked(width * height * bytesPerPixel);
        if (headerLength + pixelBytes > data.Length)
        {
            throw new InvalidDataException("PVR v2 像素数据不完整。");
        }

        byte[] rgba = DecodeMaskedPixels(
            data.Slice(headerLength, pixelBytes),
            width,
            height,
            bytesPerPixel,
            redMask,
            greenMask,
            blueMask,
            alphaMask,
            pixelType);
        if (unpremultiplyAlpha && alphaMask != 0)
        {
            Unpremultiply(rgba);
        }

        return CreateBitmap(width, height, rgba);
    }

    private static SKBitmap DecodeV3(ReadOnlySpan<byte> data, bool unpremultiplyAlpha)
    {
        ulong pixelFormat = BinaryPrimitives.ReadUInt64LittleEndian(data.Slice(8, 8));
        int height = checked((int)ReadUInt32(data, 24));
        int width = checked((int)ReadUInt32(data, 28));
        int metadataLength = checked((int)ReadUInt32(data, 48));
        int dataOffset = checked(52 + metadataLength);
        ValidateDimensions(width, height);

        if (dataOffset > data.Length || (pixelFormat >> 32) == 0)
        {
            throw new NotSupportedException("不支持的 PVR v3 压缩格式。");
        }

        Span<byte> channelNames = stackalloc byte[4];
        Span<byte> channelBits = stackalloc byte[4];
        uint names = (uint)(pixelFormat & 0xFFFFFFFF);
        uint bits = (uint)(pixelFormat >> 32);
        for (int i = 0; i < 4; i++)
        {
            channelNames[i] = (byte)(names >> (8 * i));
            channelBits[i] = (byte)(bits >> (8 * i));
        }

        int totalBits = channelBits.ToArray().Sum(value => value);
        if (totalBits is not (8 or 16 or 24 or 32))
        {
            throw new NotSupportedException($"不支持的 PVR v3 位深：{totalBits}。");
        }

        int bytesPerPixel = totalBits / 8;
        int pixelBytes = checked(width * height * bytesPerPixel);
        if (dataOffset + pixelBytes > data.Length)
        {
            throw new InvalidDataException("PVR v3 像素数据不完整。");
        }

        uint redMask = 0;
        uint greenMask = 0;
        uint blueMask = 0;
        uint alphaMask = 0;
        int shift = 0;
        for (int i = 0; i < 4 && channelBits[i] > 0; i++)
        {
            int widthInBits = channelBits[i];
            uint mask = ((1u << widthInBits) - 1u) << shift;
            switch ((char)channelNames[i])
            {
                case 'r':
                    redMask = mask;
                    break;
                case 'g':
                    greenMask = mask;
                    break;
                case 'b':
                    blueMask = mask;
                    break;
                case 'a':
                    alphaMask = mask;
                    break;
            }

            shift += widthInBits;
        }

        byte[] rgba = DecodeMaskedPixels(
            data.Slice(dataOffset, pixelBytes),
            width,
            height,
            bytesPerPixel,
            redMask,
            greenMask,
            blueMask,
            alphaMask,
            pixelType: -1);
        if (unpremultiplyAlpha && alphaMask != 0)
        {
            Unpremultiply(rgba);
        }

        return CreateBitmap(width, height, rgba);
    }

    private static byte[] DecodeMaskedPixels(
        ReadOnlySpan<byte> pixels,
        int width,
        int height,
        int bytesPerPixel,
        uint redMask,
        uint greenMask,
        uint blueMask,
        uint alphaMask,
        int pixelType)
    {
        byte[] output = GC.AllocateUninitializedArray<byte>(checked(width * height * 4));
        for (int pixelIndex = 0; pixelIndex < width * height; pixelIndex++)
        {
            int sourceOffset = pixelIndex * bytesPerPixel;
            uint value = bytesPerPixel switch
            {
                1 => pixels[sourceOffset],
                2 => BinaryPrimitives.ReadUInt16LittleEndian(pixels.Slice(sourceOffset, 2)),
                3 => (uint)(pixels[sourceOffset] |
                            (pixels[sourceOffset + 1] << 8) |
                            (pixels[sourceOffset + 2] << 16)),
                4 => BinaryPrimitives.ReadUInt32LittleEndian(pixels.Slice(sourceOffset, 4)),
                _ => throw new InvalidOperationException("无效的像素位深。"),
            };

            int targetOffset = pixelIndex * 4;
            if (bytesPerPixel == 1 && redMask == 0 && greenMask == 0 && blueMask == 0)
            {
                if (alphaMask != 0 || pixelType == 0x1B)
                {
                    output[targetOffset] = 255;
                    output[targetOffset + 1] = 255;
                    output[targetOffset + 2] = 255;
                    output[targetOffset + 3] = (byte)value;
                }
                else
                {
                    output[targetOffset] = (byte)value;
                    output[targetOffset + 1] = (byte)value;
                    output[targetOffset + 2] = (byte)value;
                    output[targetOffset + 3] = 255;
                }

                continue;
            }

            output[targetOffset] = Expand(value, redMask, fallback: 0);
            output[targetOffset + 1] = Expand(value, greenMask, fallback: 0);
            output[targetOffset + 2] = Expand(value, blueMask, fallback: 0);
            output[targetOffset + 3] = Expand(value, alphaMask, fallback: 255);
        }

        return output;
    }

    private static byte Expand(uint value, uint mask, byte fallback)
    {
        if (mask == 0)
        {
            return fallback;
        }

        int shift = BitOperations.TrailingZeroCount(mask);
        uint maximum = mask >> shift;
        uint component = (value & mask) >> shift;
        return (byte)((component * 255u + maximum / 2u) / maximum);
    }

    private static void Unpremultiply(Span<byte> rgba)
    {
        for (int offset = 0; offset < rgba.Length; offset += 4)
        {
            byte alpha = rgba[offset + 3];
            if (alpha is 0 or 255)
            {
                continue;
            }

            rgba[offset] = UnpremultiplyComponent(rgba[offset], alpha);
            rgba[offset + 1] = UnpremultiplyComponent(rgba[offset + 1], alpha);
            rgba[offset + 2] = UnpremultiplyComponent(rgba[offset + 2], alpha);
        }
    }

    private static byte UnpremultiplyComponent(byte component, byte alpha) =>
        (byte)Math.Min(255, (component * 255 + alpha / 2) / alpha);

    private static SKBitmap CreateBitmap(int width, int height, byte[] rgba)
    {
        var bitmap = new SKBitmap(width, height, SKColorType.Rgba8888, SKAlphaType.Unpremul);
        Marshal.Copy(rgba, 0, bitmap.GetPixels(), rgba.Length);
        return bitmap;
    }

    private static uint ReadUInt32(ReadOnlySpan<byte> data, int offset) =>
        BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset, 4));

    private static void ValidateDimensions(int width, int height)
    {
        if (width <= 0 || height <= 0 || width > 32768 || height > 32768)
        {
            throw new InvalidDataException($"PVR 尺寸无效：{width} x {height}。");
        }
    }
}
