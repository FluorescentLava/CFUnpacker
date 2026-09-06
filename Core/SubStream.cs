namespace CFUnpacker.Core;

/// <summary>
/// 在一个文件上开只读窗口的流。.apks/.xapk 里的内层 APK 通常以 STORED 方式存放，
/// 其数据在外层文件中是连续区间——用窗口流直接当 ZipArchive 的输入，
/// 避免把几百 MB 的 base.apk 再复制一份到磁盘。
/// 实例非线程安全，每个 worker 各自创建。
/// </summary>
internal sealed class SubStream : Stream
{
    private readonly FileStream _file;
    private readonly long _offset;
    private readonly long _length;
    private long _position;

    public SubStream(FileStream file, long offset, long length)
    {
        if (offset < 0 || length < 0 || offset + length > file.Length)
        {
            throw new ArgumentOutOfRangeException(nameof(offset));
        }

        _file = file;
        _offset = offset;
        _length = length;
    }

    public override bool CanRead => true;
    public override bool CanSeek => true;
    public override bool CanWrite => false;
    public override long Length => _length;

    public override long Position
    {
        get => _position;
        set => Seek(value, SeekOrigin.Begin);
    }

    public override int Read(byte[] buffer, int offset, int count)
    {
        long remaining = _length - _position;
        if (remaining <= 0)
        {
            return 0;
        }

        int clamped = (int)Math.Min(count, remaining);
        _file.Position = _offset + _position;
        int read = _file.Read(buffer, offset, clamped);
        _position += read;
        return read;
    }

    public override long Seek(long offset, SeekOrigin origin)
    {
        long target = origin switch
        {
            SeekOrigin.Begin => offset,
            SeekOrigin.Current => _position + offset,
            SeekOrigin.End => _length + offset,
            _ => throw new ArgumentOutOfRangeException(nameof(origin)),
        };
        if (target < 0)
        {
            throw new IOException("试图在窗口流之前定位。");
        }

        _position = target;
        return _position;
    }

    public override void Flush()
    {
    }

    public override void SetLength(long value) => throw new NotSupportedException();
    public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
}
