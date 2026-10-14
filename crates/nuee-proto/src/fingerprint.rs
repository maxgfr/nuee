//! Six-emoji fingerprint of a coordinator identity (48 bits), compared aloud by humans.

use crate::id::EndpointId;

/// 256 distinct single-scalar emoji, indexed by one byte each.
pub const EMOJI: [&str; 256] = [
    "🐵", "🐶", "🐺", "🦊", "🦝", "🐱", "🦁", "🐯", "🐴", "🦄", "🦓", "🦌", "🐮", "🐷", "🐗", "🐭",
    "🐰", "🦔", "🦇", "🐻", "🐨", "🐼", "🦥", "🦦", "🦨", "🦘", "🦡", "🐾", "🦃", "🐔", "🐣", "🐧",
    "🦅", "🦆", "🦢", "🦉", "🦩", "🦚", "🦜", "🐸", "🐊", "🐢", "🦎", "🐍", "🐲", "🦖", "🐳", "🐬",
    "🦭", "🐟", "🐡", "🦈", "🐙", "🐚", "🐌", "🦋", "🐛", "🐜", "🐝", "🐞", "🦂", "🦠", "💐", "🌸",
    "🌹", "🌺", "🌻", "🌷", "🌱", "🌲", "🌳", "🌴", "🌵", "🌾", "🌿", "🍀", "🍁", "🍄", "🍇", "🍉",
    "🍊", "🍋", "🍌", "🍍", "🥭", "🍎", "🍐", "🍑", "🍒", "🍓", "🥝", "🍅", "🥥", "🥑", "🍆", "🥔",
    "🥕", "🌽", "🥒", "🥦", "🧄", "🧅", "🥜", "🌰", "🍞", "🥐", "🥖", "🥨", "🥞", "🧇", "🧀", "🍗",
    "🥓", "🍔", "🍟", "🍕", "🌭", "🥪", "🌮", "🥚", "🍳", "🍲", "🥗", "🍿", "🧂", "🥫", "🍱", "🍙",
    "🍜", "🍝", "🍣", "🍤", "🍡", "🥠", "🦀", "🦐", "🦪", "🍦", "🍩", "🍪", "🎂", "🧁", "🥧", "🍫",
    "🍬", "🍭", "🍯", "🍼", "🥛", "☕", "🍾", "🍷", "🍸", "🍺", "🥤", "🧊", "🌍", "🌋", "🗻", "🏠",
    "🏰", "🗼", "🗽", "⛪", "🕌", "⛲", "⛺", "🌁", "🌃", "🌉", "🎠", "🎡", "🎢", "💈", "🎪", "🚂",
    "🚌", "🚑", "🚒", "🚓", "🚕", "🚗", "🚜", "🛵", "🚲", "🛴", "🛹", "⛽", "🚨", "🚥", "🛑", "🚧",
    "⚓", "⛵", "🛶", "🚤", "🚢", "🚁", "🚀", "🛸", "⌛", "⏰", "🌑", "🌙", "🌞", "⭐", "🎨", "🎲",
    "⛅", "🌀", "🌈", "🌂", "☔", "⚡", "⛄", "🔥", "💧", "🌊", "🎃", "🎄", "🎆", "🧨", "🎈", "🎉",
    "🎀", "🎁", "🎫", "🏆", "🏅", "⚽", "⚾", "🧩", "🏀", "🏐", "🏈", "🏉", "🎾", "🥏", "🎳", "🏏",
    "🎮", "🏒", "🥍", "🏓", "🏸", "🥊", "🥋", "🥅", "⛳", "🎣", "🤿", "🎽", "🎿", "🛷", "🥌", "🎯",
];

/// Six emoji derived from the first 6 bytes of `blake3("nuee/fingerprint/0" || id)`.
pub fn fingerprint(id: &EndpointId) -> [&'static str; 6] {
    let mut h = blake3::Hasher::new();
    h.update(b"nuee/fingerprint/0");
    h.update(id.as_bytes());
    let digest = h.finalize();
    let b = digest.as_bytes();
    [
        EMOJI[usize::from(b[0])],
        EMOJI[usize::from(b[1])],
        EMOJI[usize::from(b[2])],
        EMOJI[usize::from(b[3])],
        EMOJI[usize::from(b[4])],
        EMOJI[usize::from(b[5])],
    ]
}

/// The fingerprint as one space-separated string.
pub fn fingerprint_string(id: &EndpointId) -> String {
    fingerprint(id).join(" ")
}
