use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    pub parts: [u32; 4],
}

pub const CARROT1_PVR: Key = Key {
    name: "保卫萝卜 key",
    parts: [0x89427C80, 0xF850A6D9, 0x14FB3BA0, 0x3A557437],
};
pub const CARROT2_PVR: Key = Key {
    name: "保卫萝卜2 key",
    parts: [0x67AA748A, 0xFB868651, 0xE8243360, 0x34062D80],
};
pub const CARROT3_PVR: Key = Key {
    name: "保卫萝卜3/阿波 key",
    parts: [0xA8AC7F50, 0x63F379E7, 0x06AE82BA, 0x5405FE14],
};
pub const CARROT4_PVR: Key = Key {
    name: "保卫萝卜4 key",
    parts: [0x3CE64A05, 0x81E437A2, 0x37DB91EC, 0x65FA03B8],
};
pub const CARROT3_PLIST: Key = Key {
    name: "czzf plist key",
    parts: [0x26AB1359, 0x1C2485A3, 0xF2B34691, 0xAA172AF6],
};
pub const CARROT4_PLIST: Key = Key {
    name: "ff db ff ee 66 key",
    parts: [0x43F21B68, 0x9A0F3610, 0x3AC65312, 0xB8926AA3],
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Game {
    Carrot1,
    Carrot2,
    Carrot3,
    Carrot4,
    AboAdventure,
}

impl Game {
    pub const ALL: [Self; 5] = [
        Self::Carrot1,
        Self::Carrot2,
        Self::Carrot3,
        Self::Carrot4,
        Self::AboAdventure,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Carrot1 => "保卫萝卜",
            Self::Carrot2 => "保卫萝卜2",
            Self::Carrot3 => "保卫萝卜3",
            Self::Carrot4 => "保卫萝卜4",
            Self::AboAdventure => "保卫萝卜阿波之旅",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Carrot1 => "cf1",
            Self::Carrot2 => "cf2",
            Self::Carrot3 => "cf3",
            Self::Carrot4 => "cf4",
            Self::AboAdventure => "abo",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|game| {
            id.eq_ignore_ascii_case(game.id())
                || id == game.display_name()
                || match game {
                    Self::Carrot1 => id.eq_ignore_ascii_case("carrot1"),
                    Self::Carrot2 => id.eq_ignore_ascii_case("carrot2"),
                    Self::Carrot3 => id.eq_ignore_ascii_case("carrot3"),
                    Self::Carrot4 => id.eq_ignore_ascii_case("carrot4"),
                    Self::AboAdventure => id.eq_ignore_ascii_case("aboadventure"),
                }
        })
    }

    pub fn pvr_keys(self) -> &'static [Key] {
        match self {
            Self::Carrot1 => &[CARROT1_PVR, CARROT2_PVR],
            Self::Carrot2 => &[CARROT2_PVR],
            Self::Carrot3 | Self::AboAdventure => &[CARROT3_PVR],
            Self::Carrot4 => &[CARROT4_PVR, CARROT3_PVR],
        }
    }

    pub fn plist_key(self) -> Option<Key> {
        match self {
            Self::Carrot1 | Self::Carrot2 => None,
            Self::Carrot3 | Self::AboAdventure => Some(CARROT3_PLIST),
            Self::Carrot4 => Some(CARROT4_PLIST),
        }
    }

    pub fn plist_rounds(self) -> u32 {
        if self == Self::Carrot4 { 8 } else { 6 }
    }

    /// Wrapped standalone images can use either series' key independently of
    /// their containing APK. Prefer the selected game's key, then compatibility
    /// keys, keeping their custom-wrapper round counts separate from CCZ's six.
    pub fn wrapper_keys(self) -> Vec<(Key, u32)> {
        let mut keys = Vec::with_capacity(2);
        if let Some(key) = self.plist_key() {
            keys.push((key, self.plist_rounds()));
        }
        for candidate in [(CARROT4_PLIST, 8), (CARROT3_PLIST, 6)] {
            if !keys.contains(&candidate) {
                keys.push(candidate);
            }
        }
        keys
    }

    pub fn unpremultiply(self) -> bool {
        self != Self::Carrot1
    }
}

impl fmt::Display for Game {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_keys_prefer_profile_and_include_other_round_counts_once() {
        assert_eq!(
            Game::Carrot3.wrapper_keys(),
            [(CARROT3_PLIST, 6), (CARROT4_PLIST, 8)]
        );
        assert_eq!(
            Game::Carrot4.wrapper_keys(),
            [(CARROT4_PLIST, 8), (CARROT3_PLIST, 6)]
        );
        assert_eq!(
            Game::Carrot1.wrapper_keys(),
            [(CARROT4_PLIST, 8), (CARROT3_PLIST, 6)]
        );
    }
}
