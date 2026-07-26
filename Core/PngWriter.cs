using System.Buffers;
using System.Buffers.Binary;
using System.IO.Compression;

namespace CarrotUnpacker.Core;

internal static class PngWriter
{
    private static readonly byte[] Signature = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    private static readonly uint[][] CrcTables = CreateCrcTables();

    public static void Write(string path, int width, int height, ReadOnlySpan<byte> rgba)
    {
        int rowLength = checked(width * 4);
        if (rgba.Length != checked(rowLength * height))
        {
            throw new ArgumentException("RGBA data length does not match the image dimensions.", nameof(rgba));
        }

        using var output = new FileStream(
            path,
            new FileStreamOptions
            {
                Mode = FileMode.Create,
                Access = FileAccess.Write,
                Share = FileShare.None,
                BufferSize = 16 * 1024,
                Options = FileOptions.SequentialScan,
            });
        output.Write(Signature);

        Span<byte> header = stackalloc byte[13];
        BinaryPrimitives.WriteUInt32BigEndian(header[..4], (uint)width);
        BinaryPrimitives.WriteUInt32BigEndian(header.Slice(4, 4), (uint)height);
        header[8] = 8;
        header[9] = 6;
        header[10] = 0;
        header[11] = 0;
        header[12] = 0;
        WriteChunk(output, "IHDR"u8, header);

        using (var idat = new IdatStream(output))
        {
            using (var zlib = new ZLibStream(idat, CompressionLevel.Fastest, leaveOpen: true))
            {
                int scanlineLength = checked(rgba.Length + height);
                byte[] scanlines = ArrayPool<byte>.Shared.Rent(scanlineLength);
                try
                {
                    int sourceOffset = 0;
                    int destinationOffset = 0;
                    for (int row = 0; row < height; row++)
                    {
                        scanlines[destinationOffset++] = 0;
                        rgba.Slice(sourceOffset, rowLength)
                            .CopyTo(scanlines.AsSpan(destinationOffset, rowLength));
                        sourceOffset += rowLength;
                        destinationOffset += rowLength;
                    }

                    zlib.Write(scanlines.AsSpan(0, scanlineLength));
                }
                finally
                {
                    ArrayPool<byte>.Shared.Return(scanlines);
                }
            }

            idat.Complete();
        }

        WriteChunk(output, "IEND"u8, ReadOnlySpan<byte>.Empty);
    }

    private static void WriteChunk(Stream output, ReadOnlySpan<byte> type, ReadOnlySpan<byte> payload)
    {
        Span<byte> length = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(length, (uint)payload.Length);
        output.Write(length);
        output.Write(type);
        output.Write(payload);

        uint crc = 0xFFFFFFFF;
        crc = UpdateCrc(crc, type);
        crc = UpdateCrc(crc, payload) ^ 0xFFFFFFFF;
        Span<byte> crcBytes = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(crcBytes, crc);
        output.Write(crcBytes);
    }

    private static uint UpdateCrc(uint crc, ReadOnlySpan<byte> data)
    {
        while (data.Length >= sizeof(ulong))
        {
            uint first = BinaryPrimitives.ReadUInt32LittleEndian(data) ^ crc;
            uint second = BinaryPrimitives.ReadUInt32LittleEndian(data[4..]);
            crc =
                CrcTables[7][first & 0xFF] ^
                CrcTables[6][(first >> 8) & 0xFF] ^
                CrcTables[5][(first >> 16) & 0xFF] ^
                CrcTables[4][first >> 24] ^
                CrcTables[3][second & 0xFF] ^
                CrcTables[2][(second >> 8) & 0xFF] ^
                CrcTables[1][(second >> 16) & 0xFF] ^
                CrcTables[0][second >> 24];
            data = data[sizeof(ulong)..];
        }

        foreach (byte value in data)
        {
            crc = CrcTables[0][(crc ^ value) & 0xFF] ^ (crc >> 8);
        }

        return crc;
    }

    private static uint[][] CreateCrcTables()
    {
        var tables = new uint[8][];
        tables[0] = new uint[256];
        for (uint value = 0; value < tables[0].Length; value++)
        {
            uint current = value;
            for (int bit = 0; bit < 8; bit++)
            {
                current = (current & 1) != 0
                    ? 0xEDB88320 ^ (current >> 1)
                    : current >> 1;
            }

            tables[0][value] = current;
        }

        for (int tableIndex = 1; tableIndex < tables.Length; tableIndex++)
        {
            tables[tableIndex] = new uint[256];
            for (int value = 0; value < tables[tableIndex].Length; value++)
            {
                uint previous = tables[tableIndex - 1][value];
                tables[tableIndex][value] =
                    (previous >> 8) ^ tables[0][previous & 0xFF];
            }
        }

        return tables;
    }

    private sealed class IdatStream : Stream
    {
        private const int BufferSize = 64 * 1024;

        private readonly Stream _output;
        private byte[]? _buffer = ArrayPool<byte>.Shared.Rent(BufferSize);
        private int _length;
        private bool _completed;

        public IdatStream(Stream output) => _output = output;

        public override bool CanRead => false;
        public override bool CanSeek => false;
        public override bool CanWrite => !_completed;
        public override long Length => throw new NotSupportedException();
        public override long Position
        {
            get => throw new NotSupportedException();
            set => throw new NotSupportedException();
        }

        public void Complete()
        {
            if (_completed)
            {
                return;
            }

            FlushChunk();
            _completed = true;
        }

        public override void Flush()
        {
        }

        public override void Write(byte[] buffer, int offset, int count) =>
            Write(buffer.AsSpan(offset, count));

        public override void Write(ReadOnlySpan<byte> source)
        {
            ThrowIfCompleted();
            while (!source.IsEmpty)
            {
                byte[] buffer = _buffer!;
                if (_length == buffer.Length)
                {
                    FlushChunk();
                }

                int copyLength = Math.Min(buffer.Length - _length, source.Length);
                source[..copyLength].CopyTo(buffer.AsSpan(_length, copyLength));
                _length += copyLength;
                source = source[copyLength..];
            }
        }

        public override void WriteByte(byte value)
        {
            ThrowIfCompleted();
            byte[] buffer = _buffer!;
            if (_length == buffer.Length)
            {
                FlushChunk();
            }

            buffer[_length++] = value;
        }

        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();

        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                Complete();
                byte[]? buffer = Interlocked.Exchange(ref _buffer, null);
                if (buffer is not null)
                {
                    ArrayPool<byte>.Shared.Return(buffer);
                }
            }

            base.Dispose(disposing);
        }

        private void FlushChunk()
        {
            if (_length == 0)
            {
                return;
            }

            WriteChunk(_output, "IDAT"u8, _buffer!.AsSpan(0, _length));
            _length = 0;
        }

        private void ThrowIfCompleted()
        {
            if (_completed)
            {
                throw new ObjectDisposedException(nameof(IdatStream));
            }
        }
    }
}
