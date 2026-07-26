namespace CFUnpacker.Models;

public enum UnpackStage
{
    Preparing,
    Extracting,
    Scanning,
    Splitting,
    Finalizing,
    Completed,
}

public sealed record UnpackRequest(
    GameProfile Profile,
    string ApkPath,
    string OutputParent,
    bool OverwriteExisting);

public sealed record UnpackProgress(
    UnpackStage Stage,
    double Percent,
    string Message,
    int CompletedItems = 0,
    int TotalItems = 0);

public sealed record UnpackResult(
    string OutputPath,
    int AssetsExtracted,
    int PlistsScanned,
    int AtlasesDecoded,
    int FramesWritten,
    int SkippedItems,
    TimeSpan Elapsed,
    IReadOnlyList<string> Warnings);

internal sealed class MutableUnpackStats
{
    public int AssetsExtracted;
    public int PlistsScanned;
    public int AtlasesDecoded;
    public int FramesWritten;
    public int SkippedItems;
    public readonly System.Collections.Concurrent.ConcurrentQueue<string> Warnings = new();
    public readonly System.Collections.Concurrent.ConcurrentDictionary<string, int> KeyUsage = new();
}
