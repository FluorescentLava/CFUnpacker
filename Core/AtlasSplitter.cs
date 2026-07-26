using CarrotUnpacker.Models;
using SkiaSharp;
using System.Buffers;
using System.Runtime.InteropServices;

namespace CarrotUnpacker.Core;

internal sealed record AtlasSplitResult(
    int FramesWritten,
    string TexturePath,
    string TextureKey,
    string PlistTransform);

internal static class AtlasSplitter
{
    public static AtlasSplitResult Split(
        string plistPath,
        string extractedRoot,
        string outputPngRoot,
        GameProfile profile,
        CancellationToken cancellationToken,
        Action? frameWritten = null)
    {
        AtlasDefinition definition = PlistAtlasReader.Read(plistPath, profile) ??
                                     throw new InvalidDataException("plist 不包含可拆分的 frames。");
        string texturePath = FindTexture(plistPath, definition.TextureFileName);
        using SKBitmap atlas = DecodeTexture(texturePath, profile, out string textureKey);

        string relativePlist = Path.GetRelativePath(extractedRoot, plistPath);
        string relativeDirectory = Path.GetDirectoryName(relativePlist) ?? string.Empty;
        string atlasFolder = Path.Combine(
            outputPngRoot,
            relativeDirectory,
            Path.GetFileNameWithoutExtension(plistPath));
        byte[] atlasPixels = CopyRgba(atlas);
        var outputDirectories = new HashSet<string>(StringComparer.OrdinalIgnoreCase)
        {
            atlasFolder,
        };
        Directory.CreateDirectory(atlasFolder);

        int written = 0;
        foreach (AtlasFrame frame in definition.Frames)
        {
            cancellationToken.ThrowIfCancellationRequested();
            WriteFrame(atlasPixels, atlas.Width, atlas.Height, frame, atlasFolder, outputDirectories);
            written++;
            frameWritten?.Invoke();
        }

        return new AtlasSplitResult(written, texturePath, textureKey, definition.PlistTransform);
    }

    private static SKBitmap DecodeTexture(
        string path,
        GameProfile profile,
        out string keyName)
    {
        byte[] source = File.ReadAllBytes(path);
        if (EncryptionCodec.IsCcz(source))
        {
            byte[] pvr = EncryptionCodec.DecodeCcz(source, profile.PvrKeys, out keyName);
            bool unpremultiply = profile.UnpremultiplyPvrAlpha ||
                                 (profile.Kind == GameKind.Carrot1 &&
                                  keyName.Contains("保卫萝卜2", StringComparison.Ordinal));
            return PvrDecoder.Decode(pvr, unpremultiply);
        }

        if (source.AsSpan().StartsWith(new byte[] { 0xFF, 0xDB, 0xFF, 0xEE, 0x66 }) &&
            profile.PlistKey is not null &&
            EncryptionCodec.TryDecodeCustom(
                source,
                profile.PlistKey,
                profile.PlistKeyRounds,
                out byte[] png,
                out string transform))
        {
            keyName = $"{profile.PlistKey.Name} ({transform})";
            if (png.AsSpan().StartsWith(new byte[] { 0x89, (byte)'P', (byte)'N', (byte)'G' }))
            {
                return DecodePngUnpremultiplied(png);
            }

            return PvrDecoder.Decode(png, profile.UnpremultiplyPvrAlpha);
        }

        if (source.AsSpan().StartsWith(new byte[] { 0x89, (byte)'P', (byte)'N', (byte)'G' }))
        {
            keyName = "明文 PNG";
            return DecodePngUnpremultiplied(source);
        }

        keyName = "明文 PVR";
        return PvrDecoder.Decode(source);
    }

    private static string FindTexture(string plistPath, string? textureFileName)
    {
        string directory = Path.GetDirectoryName(plistPath)!;
        var candidates = new List<string>();
        if (!string.IsNullOrWhiteSpace(textureFileName))
        {
            string normalized = textureFileName
                .Replace('/', Path.DirectorySeparatorChar)
                .Replace('\\', Path.DirectorySeparatorChar);
            candidates.Add(Path.Combine(directory, normalized));
            if (normalized.EndsWith(".pvr", StringComparison.OrdinalIgnoreCase))
            {
                candidates.Add(Path.Combine(directory, normalized + ".ccz"));
            }
        }

        string basePath = Path.Combine(directory, Path.GetFileNameWithoutExtension(plistPath));
        candidates.Add(basePath + ".pvr.ccz");
        candidates.Add(basePath + ".png");
        candidates.Add(basePath + ".pvr");
        candidates.Add(basePath + ".ccz");

        foreach (string candidate in candidates.Distinct(StringComparer.OrdinalIgnoreCase))
        {
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }

        throw new FileNotFoundException("找不到 plist 对应的纹理文件。", plistPath);
    }

    private static void WriteFrame(
        byte[] atlasPixels,
        int atlasWidth,
        int atlasHeight,
        AtlasFrame frame,
        string atlasFolder,
        HashSet<string> outputDirectories)
    {
        IntRect sourceRect = frame.Rotated
            ? new IntRect(
                frame.Frame.X,
                frame.Frame.Y,
                frame.Frame.Height,
                frame.Frame.Width)
            : frame.Frame;
        if (sourceRect.Width <= 0 ||
            sourceRect.Height <= 0 ||
            sourceRect.X < 0 ||
            sourceRect.Y < 0 ||
            sourceRect.X + sourceRect.Width > atlasWidth ||
            sourceRect.Y + sourceRect.Height > atlasHeight)
        {
            throw new InvalidDataException(
                $"frame 越界：{frame.Name} ({sourceRect.X},{sourceRect.Y},{sourceRect.Width},{sourceRect.Height})。");
        }

        int spriteWidth = frame.Rotated ? sourceRect.Height : sourceRect.Width;
        int spriteHeight = frame.Rotated ? sourceRect.Width : sourceRect.Height;
        int canvasWidth = frame.SourceSize.Width > 0 ? frame.SourceSize.Width : spriteWidth;
        int canvasHeight = frame.SourceSize.Height > 0 ? frame.SourceSize.Height : spriteHeight;
        if (canvasWidth > 32768 || canvasHeight > 32768)
        {
            throw new InvalidDataException($"frame 尺寸无效：{frame.Name} ({canvasWidth}x{canvasHeight})。");
        }

        int pixelBufferLength = checked(canvasWidth * canvasHeight * 4);
        byte[] outputPixels = ArrayPool<byte>.Shared.Rent(pixelBufferLength);
        try
        {
            bool fillsCanvas =
                frame.SourceColorRect.X == 0 &&
                frame.SourceColorRect.Y == 0 &&
                spriteWidth == canvasWidth &&
                spriteHeight == canvasHeight;
            if (!fillsCanvas)
            {
                Array.Clear(outputPixels, 0, pixelBufferLength);
            }

            CopyFramePixels(
                atlasPixels,
                atlasWidth,
                sourceRect,
                frame.Rotated,
                spriteWidth,
                spriteHeight,
                outputPixels,
                canvasWidth,
                canvasHeight,
                frame.SourceColorRect.X,
                frame.SourceColorRect.Y);

            string outputPath = BuildSafeOutputPath(atlasFolder, frame.Name);
            string outputDirectory = Path.GetDirectoryName(outputPath)!;
            if (outputDirectories.Add(outputDirectory))
            {
                Directory.CreateDirectory(outputDirectory);
            }

            PngWriter.Write(
                outputPath,
                canvasWidth,
                canvasHeight,
                outputPixels.AsSpan(0, pixelBufferLength));
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(outputPixels);
        }
    }

    private static void CopyFramePixels(
        byte[] atlasPixels,
        int atlasWidth,
        IntRect sourceRect,
        bool rotated,
        int spriteWidth,
        int spriteHeight,
        byte[] outputPixels,
        int canvasWidth,
        int canvasHeight,
        int destinationX,
        int destinationY)
    {
        int sourceStartX = Math.Max(0, -destinationX);
        int sourceStartY = Math.Max(0, -destinationY);
        int targetStartX = Math.Max(0, destinationX);
        int targetStartY = Math.Max(0, destinationY);
        int copyWidth = Math.Min(spriteWidth - sourceStartX, canvasWidth - targetStartX);
        int copyHeight = Math.Min(spriteHeight - sourceStartY, canvasHeight - targetStartY);
        if (copyWidth <= 0 || copyHeight <= 0)
        {
            return;
        }

        if (!rotated)
        {
            int bytesPerRow = copyWidth * 4;
            for (int row = 0; row < copyHeight; row++)
            {
                int sourceOffset = ((sourceRect.Y + sourceStartY + row) * atlasWidth + sourceRect.X + sourceStartX) * 4;
                int destinationOffset = ((targetStartY + row) * canvasWidth + targetStartX) * 4;
                atlasPixels.AsSpan(sourceOffset, bytesPerRow)
                    .CopyTo(outputPixels.AsSpan(destinationOffset, bytesPerRow));
            }

            return;
        }

        ReadOnlySpan<uint> sourcePixels = MemoryMarshal.Cast<byte, uint>(atlasPixels);
        Span<uint> destinationPixels = MemoryMarshal.Cast<byte, uint>(
            outputPixels.AsSpan(0, checked(canvasWidth * canvasHeight * 4)));
        for (int spriteY = sourceStartY; spriteY < sourceStartY + copyHeight; spriteY++)
        {
            for (int spriteX = sourceStartX; spriteX < sourceStartX + copyWidth; spriteX++)
            {
                int atlasX = sourceRect.X + sourceRect.Width - 1 - spriteY;
                int atlasY = sourceRect.Y + spriteX;
                int sourceIndex = atlasY * atlasWidth + atlasX;
                int destinationIndex =
                    (targetStartY + spriteY - sourceStartY) * canvasWidth +
                    targetStartX +
                    spriteX -
                    sourceStartX;
                destinationPixels[destinationIndex] = sourcePixels[sourceIndex];
            }
        }
    }

    private static byte[] CopyRgba(SKBitmap bitmap)
    {
        if (bitmap.ColorType != SKColorType.Rgba8888 || bitmap.AlphaType != SKAlphaType.Unpremul)
        {
            throw new InvalidDataException("内部位图格式不是 RGBA8888/Unpremul。");
        }

        var result = GC.AllocateUninitializedArray<byte>(checked(bitmap.Width * bitmap.Height * 4));
        IntPtr pixels = bitmap.GetPixels();
        int targetOffset = 0;
        for (int row = 0; row < bitmap.Height; row++)
        {
            Marshal.Copy(IntPtr.Add(pixels, row * bitmap.RowBytes), result, targetOffset, bitmap.Width * 4);
            targetOffset += bitmap.Width * 4;
        }

        return result;
    }

    private static SKBitmap DecodePngUnpremultiplied(byte[] data)
    {
        using var stream = new SKMemoryStream(data);
        using SKCodec codec = SKCodec.Create(stream) ??
                              throw new InvalidDataException("PNG 无法读取。");
        var info = new SKImageInfo(
            codec.Info.Width,
            codec.Info.Height,
            SKColorType.Rgba8888,
            SKAlphaType.Unpremul);
        var bitmap = new SKBitmap(info);
        SKCodecResult result = codec.GetPixels(info, bitmap.GetPixels());
        if (result is not (SKCodecResult.Success or SKCodecResult.IncompleteInput))
        {
            bitmap.Dispose();
            throw new InvalidDataException($"PNG 解码失败：{result}。");
        }

        return bitmap;
    }

    private static string BuildSafeOutputPath(string atlasFolder, string frameName)
    {
        string normalized = frameName.Replace('\\', '/').TrimStart('/');
        string[] segments = normalized
            .Split('/', StringSplitOptions.RemoveEmptyEntries)
            .Where(segment => segment is not "." and not "..")
            .Select(SanitizeSegment)
            .ToArray();

        if (segments.Length == 0)
        {
            throw new InvalidDataException("plist 中包含空 frame 名称。");
        }

        string relative = Path.Combine(segments);
        if (!Path.HasExtension(relative))
        {
            relative += ".png";
        }

        string fullPath = Path.GetFullPath(Path.Combine(atlasFolder, relative));
        string safeRoot = Path.GetFullPath(atlasFolder) + Path.DirectorySeparatorChar;
        if (!fullPath.StartsWith(safeRoot, StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidDataException($"frame 路径越界：{frameName}。");
        }

        return fullPath;
    }

    private static string SanitizeSegment(string segment)
    {
        char[] invalid = Path.GetInvalidFileNameChars();
        string sanitized = string.Concat(segment.Select(character => invalid.Contains(character) ? '_' : character));
        return string.IsNullOrWhiteSpace(sanitized) ? "_" : sanitized;
    }
}
