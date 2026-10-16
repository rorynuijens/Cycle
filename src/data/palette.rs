//! Every colour the 3D view draws, by name.
//!
//! The scene is a bright, low-poly game world (the stylized-route skill, §3):
//! no mesh builder or shader names a colour of its own, they all come from
//! here. A biome (`data/biome.rs`) overrides some names with its own values;
//! the road, its paint and the outline never change, so the route always
//! reads as the same track.
//!
//! Values are the display colours as written in hex: the view draws into an
//! ordinary 8-bit framebuffer and lights them by multiplying, so `#7BC950`
//! in full sun is `#7BC950` on screen. Fixed rather than theme-derived, the
//! same exemption the 2D map takes for its tiles (CLAUDE.md §1.6).

/// Red, green, blue, each 0–1.
pub type Rgb = [f32; 3];

/// `0xRRGGBB` as [`Rgb`].
pub const fn hex(v: u32) -> Rgb {
    [
        ((v >> 16) & 0xFF) as f32 / 255.0,
        ((v >> 8) & 0xFF) as f32 / 255.0,
        (v & 0xFF) as f32 / 255.0,
    ]
}

/// `#rrggbb` as [`Rgb`]; `None` for anything else. For colours read from
/// the biome data file.
pub fn parse(text: &str) -> Option<Rgb> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok().map(hex)
}

/// One full set of named colours. `repr(C)`: a test walks the fields in order.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub grass_low: Rgb,
    pub grass_high: Rgb,
    pub rock: Rgb,
    pub snow: Rgb,
    pub road: Rgb,
    pub road_edge: Rgb,
    /// The dashed line down the middle of the track.
    pub road_line: Rgb,
    pub side_road: Rgb,
    /// Bridge decks, parapets, pillars and tunnel walls.
    pub concrete: Rgb,
    pub water: Rgb,
    pub tree_a: Rgb,
    pub tree_b: Rgb,
    pub tree_c: Rgb,
    pub trunk: Rgb,
    pub wall_a: Rgb,
    pub wall_b: Rgb,
    pub wall_c: Rgb,
    pub roof_a: Rgb,
    pub roof_b: Rgb,
    pub roof_c: Rgb,
    pub sky_top: Rgb,
    /// The sky at the horizon, which is also the colour of the haze.
    pub sky_horizon: Rgb,
    pub outline: Rgb,
    /// Squares, car parks and the ground in towns.
    pub paving: Rgb,
    pub sand: Rgb,
    pub wet_sand: Rgb,
    pub field_wheat: Rgb,
    pub field_ploughed: Rgb,
    pub field_green: Rgb,
    pub sea_deep: Rgb,
    pub sea_shallow: Rgb,
    pub foam: Rgb,
    /// Sheep's faces and legs.
    pub animal_dark: Rgb,
    pub fleece_a: Rgb,
    pub fleece_b: Rgb,
    pub fleece_c: Rgb,
    pub bird: Rgb,
    /// Mountains on the horizon, and the snow on their peaks.
    pub mountain: Rgb,
    pub mountain_snow: Rgb,
    pub cloud: Rgb,
    /// The rider: helmet, hair, jersey, skin, shorts and shoes, the bike's
    /// frame, and its tyres — the default avatar (`rider::Avatar`).
    pub helmet: Rgb,
    pub hair: Rgb,
    pub jersey: Rgb,
    pub skin: Rgb,
    pub kit: Rgb,
    pub frame: Rgb,
    pub tyre: Rgb,
}

/// The shared base palette: the skill's names (§3, §7.1), tuned to the
/// bright countryside the rider chose, plus this app's own for the animals,
/// the horizon and the rider.
pub const BASE: Palette = Palette {
    grass_low: hex(0xA6EC6A),
    grass_high: hex(0x84D457),
    rock: hex(0xA79A8A),
    snow: hex(0xF2F5F7),
    road: hex(0x5C606C),
    road_edge: hex(0xF6E27A),
    road_line: hex(0xF7F7F2),
    side_road: hex(0x8A8D94),
    concrete: hex(0xC9C6BF),
    water: hex(0x5CC6F2),
    tree_a: hex(0x4FA83E),
    tree_b: hex(0x3E9440),
    tree_c: hex(0x66B845),
    trunk: hex(0x7A5134),
    wall_a: hex(0xF1E3C8),
    wall_b: hex(0xE8C9A6),
    wall_c: hex(0xD9E2EC),
    roof_a: hex(0xC8553D),
    roof_b: hex(0x8C5E58),
    roof_c: hex(0x5C6B7A),
    sky_top: hex(0x8DBDE8),
    sky_horizon: hex(0xD8ECF7),
    outline: hex(0x1E2430),
    paving: hex(0xD6D0C4),
    sand: hex(0xF3DFA8),
    wet_sand: hex(0xD9C089),
    field_wheat: hex(0xF1DE8C),
    field_ploughed: hex(0xC98A57),
    field_green: hex(0xC4EE7C),
    sea_deep: hex(0x2F8FD1),
    sea_shallow: hex(0x5FD0E3),
    foam: hex(0xFFFFFF),
    animal_dark: hex(0x2C2F38),
    fleece_a: hex(0xF4F2EA),
    fleece_b: hex(0xE9E4D4),
    fleece_c: hex(0xFFFFFF),
    bird: hex(0x3D4250),
    mountain: hex(0xB4CCE0),
    mountain_snow: hex(0xEEF4FA),
    cloud: hex(0xFFFFFF),
    helmet: hex(0xF6D84A),
    hair: hex(0x6B4428),
    jersey: hex(0xF29A3C),
    skin: hex(0xE8B48E),
    kit: hex(0x2A2E3A),
    frame: hex(0xE2463B),
    tyre: hex(0x23252C),
};

/// Names a biome may not override: the track and the rider look the same
/// everywhere.
pub const FIXED: [&str; 11] = [
    "road",
    "road_edge",
    "road_line",
    "outline",
    "helmet",
    "hair",
    "jersey",
    "skin",
    "kit",
    "frame",
    "tyre",
];

impl Palette {
    /// The colour called `name`, for a data file to address.
    fn slot(&mut self, name: &str) -> Option<&mut Rgb> {
        Some(match name {
            "grass_low" => &mut self.grass_low,
            "grass_high" => &mut self.grass_high,
            "rock" => &mut self.rock,
            "snow" => &mut self.snow,
            "road" => &mut self.road,
            "road_edge" => &mut self.road_edge,
            "road_line" => &mut self.road_line,
            "side_road" => &mut self.side_road,
            "concrete" => &mut self.concrete,
            "water" => &mut self.water,
            "tree_a" => &mut self.tree_a,
            "tree_b" => &mut self.tree_b,
            "tree_c" => &mut self.tree_c,
            "trunk" => &mut self.trunk,
            "wall_a" => &mut self.wall_a,
            "wall_b" => &mut self.wall_b,
            "wall_c" => &mut self.wall_c,
            "roof_a" => &mut self.roof_a,
            "roof_b" => &mut self.roof_b,
            "roof_c" => &mut self.roof_c,
            "sky_top" => &mut self.sky_top,
            "sky_horizon" => &mut self.sky_horizon,
            "outline" => &mut self.outline,
            "paving" => &mut self.paving,
            "sand" => &mut self.sand,
            "wet_sand" => &mut self.wet_sand,
            "field_wheat" => &mut self.field_wheat,
            "field_ploughed" => &mut self.field_ploughed,
            "field_green" => &mut self.field_green,
            "sea_deep" => &mut self.sea_deep,
            "sea_shallow" => &mut self.sea_shallow,
            "foam" => &mut self.foam,
            "animal_dark" => &mut self.animal_dark,
            "fleece_a" => &mut self.fleece_a,
            "fleece_b" => &mut self.fleece_b,
            "fleece_c" => &mut self.fleece_c,
            "bird" => &mut self.bird,
            "mountain" => &mut self.mountain,
            "mountain_snow" => &mut self.mountain_snow,
            "cloud" => &mut self.cloud,
            "helmet" => &mut self.helmet,
            "hair" => &mut self.hair,
            "jersey" => &mut self.jersey,
            "skin" => &mut self.skin,
            "kit" => &mut self.kit,
            "frame" => &mut self.frame,
            "tyre" => &mut self.tyre,
            _ => return None,
        })
    }

    /// This palette with `name` set to `rgb`, or `None` if there is no colour
    /// of that name or it is one of the [`FIXED`] track colours.
    pub fn with(mut self, name: &str, rgb: Rgb) -> Option<Self> {
        if FIXED.contains(&name) {
            return None;
        }
        *self.slot(name)? = rgb;
        Some(self)
    }

    /// A tree crown colour, picked by `seed`.
    pub fn tree(&self, seed: u64) -> Rgb {
        [self.tree_a, self.tree_b, self.tree_c][(seed % 3) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_hex_exactly_at_both_ends_of_a_channel() {
        assert_eq!(hex(0x000000), [0.0, 0.0, 0.0]);
        assert_eq!(hex(0xFFFFFF), [1.0, 1.0, 1.0]);
        assert_eq!(hex(0xFF0080), [1.0, 0.0, 128.0 / 255.0]);
    }

    #[test]
    fn should_hold_the_skills_values_by_name() {
        assert_eq!(BASE.grass_low, hex(0xA6EC6A));
        assert_eq!(BASE.road_edge, hex(0xF6E27A));
        assert_eq!(BASE.sky_horizon, hex(0xD8ECF7));
        assert_eq!(BASE.outline, hex(0x1E2430));
    }

    #[test]
    fn should_parse_only_six_digit_hex() {
        assert_eq!(parse("#7BC950"), Some(hex(0x7BC950)));
        assert_eq!(parse("#7bc950"), Some(hex(0x7BC950)));
        for bad in [
            "7BC950", "#7BC95", "#7BC9500", "#7BC95G", "", "#", "#+7BC95",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn should_override_a_name_and_nothing_else() {
        let red = hex(0xFF0000);
        let p = BASE.with("grass_low", red).expect("a known name");
        assert_eq!(p.grass_low, red);
        assert_eq!(p.with("grass_low", BASE.grass_low), Some(BASE));
    }

    #[test]
    fn should_reach_every_colour_by_its_name() {
        // Every field, each with a different value, set by name: a field the
        // name table forgot keeps the base value and shows here.
        const NAMES: [&str; 47] = [
            "grass_low",
            "grass_high",
            "rock",
            "snow",
            "road",
            "road_edge",
            "road_line",
            "side_road",
            "concrete",
            "water",
            "tree_a",
            "tree_b",
            "tree_c",
            "trunk",
            "wall_a",
            "wall_b",
            "wall_c",
            "roof_a",
            "roof_b",
            "roof_c",
            "sky_top",
            "sky_horizon",
            "outline",
            "paving",
            "sand",
            "wet_sand",
            "field_wheat",
            "field_ploughed",
            "field_green",
            "sea_deep",
            "sea_shallow",
            "foam",
            "animal_dark",
            "fleece_a",
            "fleece_b",
            "fleece_c",
            "bird",
            "mountain",
            "mountain_snow",
            "cloud",
            "helmet",
            "hair",
            "jersey",
            "skin",
            "kit",
            "frame",
            "tyre",
        ];
        assert_eq!(NAMES.len(), size_of::<Palette>() / size_of::<Rgb>());
        let mut p = BASE;
        for (k, name) in NAMES.iter().enumerate() {
            *p.slot(name).unwrap_or_else(|| panic!("{name} has no slot")) = [k as f32; 3];
        }
        let flat: [Rgb; 47] = unsafe { std::mem::transmute(p) };
        for (k, rgb) in flat.iter().enumerate() {
            assert_eq!(*rgb, [k as f32; 3], "field {k} is not reached by its name");
        }
    }

    #[test]
    fn should_refuse_an_unknown_name_or_a_track_colour() {
        assert_eq!(BASE.with("grass", [0.0; 3]), None);
        for name in FIXED {
            assert_eq!(BASE.with(name, [0.0; 3]), None, "{name}");
        }
    }

    #[test]
    fn should_pick_every_variant_by_seed() {
        let mut seen: Vec<Rgb> = (0..3).map(|s| BASE.tree(s)).collect();
        seen.dedup();
        assert_eq!(seen.len(), 3);
        assert_eq!(BASE.tree(u64::MAX), BASE.tree(u64::MAX));
    }
}
