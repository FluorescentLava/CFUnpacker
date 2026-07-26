namespace CarrotUnpacker.Models;

public enum GameKind
{
    Carrot1,
    Carrot2,
    Carrot3,
    Carrot4,
    AboAdventure,
}

public sealed record EncryptionKey(string Name, uint Part0, uint Part1, uint Part2, uint Part3)
{
    public uint[] Parts => [Part0, Part1, Part2, Part3];
}

public sealed record GameProfile(
    GameKind Kind,
    string DisplayName,
    IReadOnlyList<EncryptionKey> PvrKeys,
    EncryptionKey? PlistKey = null,
    int PlistKeyRounds = 6,
    bool UnpremultiplyPvrAlpha = false)
{
    private static readonly EncryptionKey Carrot1Pvr =
        new("保卫萝卜 key", 0x89427C80, 0xF850A6D9, 0x14FB3BA0, 0x3A557437);

    private static readonly EncryptionKey Carrot2Pvr =
        new("保卫萝卜2 key", 0x67AA748A, 0xFB868651, 0xE8243360, 0x34062D80);

    private static readonly EncryptionKey Carrot3Pvr =
        new("保卫萝卜3/阿波 key", 0xA8AC7F50, 0x63F379E7, 0x06AE82BA, 0x5405FE14);

    private static readonly EncryptionKey Carrot3Plist =
        new("czzf plist key", 0x26AB1359, 0x1C2485A3, 0xF2B34691, 0xAA172AF6);

    private static readonly EncryptionKey Carrot4Pvr =
        new("保卫萝卜4 key", 0x3CE64A05, 0x81E437A2, 0x37DB91EC, 0x65FA03B8);

    private static readonly EncryptionKey Carrot4Plist =
        new("ff db ff ee 66 key", 0x43F21B68, 0x9A0F3610, 0x3AC65312, 0xB8926AA3);

    public static IReadOnlyList<GameProfile> All { get; } =
    [
        new(GameKind.Carrot1, "保卫萝卜", [Carrot1Pvr, Carrot2Pvr]),
        new(GameKind.Carrot2, "保卫萝卜2", [Carrot2Pvr], UnpremultiplyPvrAlpha: true),
        new(GameKind.Carrot3, "保卫萝卜3", [Carrot3Pvr], Carrot3Plist, UnpremultiplyPvrAlpha: true),
        new(GameKind.Carrot4, "保卫萝卜4", [Carrot4Pvr, Carrot3Pvr], Carrot4Plist, 8, true),
        new(GameKind.AboAdventure, "保卫萝卜阿波之旅", [Carrot3Pvr], Carrot3Plist, UnpremultiplyPvrAlpha: true),
    ];

    public static GameProfile For(GameKind kind) => All.Single(profile => profile.Kind == kind);

    public override string ToString() => DisplayName;
}
