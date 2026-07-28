/// Creative session names assigned when the user doesn't provide one (interactive agents).
const RANDOM_NAMES: &[&str] = &[
    "liquidambar",
    "wollemia",
    "metasequoia",
    "paulownia",
    "liriodendron",
    "cryptomeria",
    "cunninghamia",
    "nothofagus",
    "podocarpus",
    "fitzroya",
    "cephalotaxus",
    "taiwania",
    "sciadopitys",
    "toona",
    "cedrus",
    "sequoia",
    "juniperus",
    "stereum",
    "larix",
    "carpinus",
    "castanea",
    "aesculus",
    "juglans",
    "platanus",
    "agaricus",
    "araucaria",
    "zelkova",
    "magnolia",
    "ginkgo",
    "quercus",
    "amanita",
    "boletus",
    "morchella",
    "cantharellus",
    "pleurotus",
    "ganoderma",
    "lentinula",
    "psilocybe",
    "coprinus",
    "hydnum",
    "trametes",
    "russula",
    "lactarius",
    "populus",
    "laricifomes",
    "cordyceps",
    "hericium",
    "laetiporus",
    "armillaria",
    "clavaria",
    "geastrum",
    "lycoperdon",
    "mycena",
    "marasmius",
    "cortinarius",
    "hygrocybe",
    "xylaria",
    "fistulina",
    "grifola",
    "stereum",
    "daedalea",
    "clitocybe",
    "inocybe",
    "pholiota",
    "stropharia",
    "suillus",
    "omphalotus",
    "sparassis",
    "calvatia",
    "phallus",
];

/// Session names for background/scheduled agents (weather/nature terms).
const BACKGROUND_NAMES: &[&str] = &[
    "foehn",
    "mistral",
    "tramontana",
    "galerna",
    "fitoncida",
    "espora",
    "micela",
    "rizoma",
    "lignina",
    "tanino",
    "resina",
    "humus",
];

/// Session names for raw terminal sessions (minerals).
const TERMINAL_NAMES: &[&str] = &[
    "feldspato",
    "cuarzo",
    "olivino",
    "piroxeno",
    "anfíbol",
    "biotita",
    "moscovita",
    "clorita",
    "caolinita",
    "illita",
    "esmectita",
    "vermiculita",
    "haloisita",
    "sepiolita",
    "palygorskita",
    "bario",
    "estroncio",
    "rubidio",
    "vanadio",
    "cobalto",
    "molibdeno",
    "niquel",
    "cesio",
];

/// Pick a name from `names` that isn't already in `existing`.
///
/// First tries each name bare.  On collision appends `-2`, `-3`, …
/// Falls back to a UUID-based ID if every combination is taken.
fn pick_name_from(names: &[&str], existing: &[&str]) -> String {
    use rand::prelude::IndexedRandom;

    // First try: pick a random bare name that isn't in use
    let available: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !existing.contains(n))
        .collect();
    if let Some(&name) = available.choose(&mut rand::rng()) {
        return name.to_string();
    }

    // Second try: pick a random base name and try <name>-2, <name>-3, …
    if let Some(&base) = names.choose(&mut rand::rng()) {
        for n in 2..=999u32 {
            let candidate = format!("{}-{}", base, n);
            if !existing.contains(&candidate.as_str()) {
                return candidate;
            }
        }
    }

    format!("session-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

/// Pick a random name for interactive agents (trees + fungi).
pub fn pick_random_name(existing: &[&str]) -> String {
    pick_name_from(RANDOM_NAMES, existing)
}

/// Pick a name for terminal sessions (minerals).
pub fn pick_terminal_name(existing: &[&str]) -> String {
    pick_name_from(TERMINAL_NAMES, existing)
}

/// Pick a name for background/scheduled agents (weather/nature terms).
#[allow(dead_code)]
pub fn pick_background_name(existing: &[&str]) -> String {
    pick_name_from(BACKGROUND_NAMES, existing)
}
