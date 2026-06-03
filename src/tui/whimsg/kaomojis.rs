//! Whimsical personality — animated kaomoji + contextual messages.
//!
//! Animation cycle:
//!   "harness-canopy" erases right-to-left → kaomoji flashes →
//!   message types left-to-right → holds 5-9s → erases right-to-left →
//!   brief blank → "harness-canopy" returns.

// ── Timing ────────────────────────────────────────────────────────

pub const TITLE: &str = "harness-canopy";
pub(crate) const ERASE_MS: u64 = 35;
pub(crate) const TYPE_MS: u64 = 45;
pub(crate) const KAOMOJI_MS: u64 = 400;
pub(crate) const BLANK_MS: u64 = 200;
pub(crate) const HOLD_MIN: u64 = 4;
pub(crate) const HOLD_MAX: u64 = 8;
pub(crate) const INTERVAL_MIN: u64 = 60;
pub(crate) const INTERVAL_MAX: u64 = 180;
pub(crate) const EVENT_DECAY_SECS: u64 = 15;

// ── Kaomojis ──────────────────────────────────────────────────────

pub(crate) const KAO_LOADING: &[&str] = &[
    "(Ծ‸ Ծ)",
    "( ≖.≖)",
    "(◡̀_◡́)",
    "(ㆆ_ㆆ)",
    "(◉̃_᷅◉)",
    "(͠◉_◉᷅ )",
    "(◑_◑)",
    "◌◎◍",
    "(ง'̀-'́)ง",
    "(っ◕‿◕)っ",
    "(づ ◕‿◕ )づ",
    "(๑•̀ㅂ•́)و",
];
pub(crate) const KAO_SUCCESS: &[&str] = &[
    "(*^‿^*)",
    "(◕‿◕)",
    "(っ▀¯▀)つ",
    "ヾ(´〇`)ﾉ♪♪♪",
    "(◠﹏◠)",
    "٩(˘◡˘)۶",
    "ᕙ(`▿´)ᕗ",
    "(ᵔᵕᵔ)",
    "(๑˃ᴗ˂)ﻭ",
    "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧",
    "(b ᵔ▽ᵔ)b",
    "٩(◕‿◕)۶",
    "(★ω★)",
];
pub(crate) const KAO_ERROR: &[&str] = &[
    "ಥ_ಥ",
    "◔_◔",
    "(҂◡_◡)",
    "(Ծ‸ Ծ)",
    "¯\\_(ツ)_/¯",
    "¿ⓧ_ⓧﮌ",
    "(╥﹏╥)",
    "( ˘︹˘ )",
    "(ノಠ益ಠ)ノ彡┻━┻",
    "(╯°□°）╯︵ ┻━┻",
    "(ಥ﹏ಥ)",
    "(×_×)",
];
pub(crate) const KAO_THINKING: &[&str] = &[
    "(ʘ_ʘ)",
    "(º_º)",
    "(￢_￢)",
    "(._.)",
    "ఠ_ఠ",
    "(⊙_◎)",
    "（´ー｀）",
    "(꜆꜄ * )꜆꜄",
    "( • ̀ω•́ )✧",
    "(￣ω￣;)",
    "( ˘▽˘)っ旦",
    "( ͡° ͜ʖ ͡°)",
];

// ── Actions ───────────────────────────────────────────────────────

pub(crate) const ACT_LOADING: &[&str] = &[
    "Calibrating",
    "Aligning",
    "Resolving",
    "Processing",
    "Exploring",
    "Parsing",
    "Synchronizing",
    "Mapping",
    "Scanning",
    "Warming up",
    "Hydrating",
    "Provisioning",
    "Bootstrapping",
    "Refactoring",
    "Overclocking",
    "Transpiling",
    "Grokking",
    "Defragmenting",
];
pub(crate) const ACT_SUCCESS: &[&str] = &[
    "Completed",
    "Done",
    "Stabilized",
    "Resolved",
    "Deployed",
    "Confirmed",
    "Verified",
    "Shipped",
    "Unlocked",
    "Optimized",
    "Synthesized",
    "Propagated",
    "Harmonized",
    "Ascended",
];
pub(crate) const ACT_ERROR: &[&str] = &[
    "Something broke",
    "Signal lost",
    "Unexpected anomaly",
    "Collision detected",
    "Entropy overflow",
    "Segfault in",
    "Desynchronized",
    "Depleted",
    "Terminated",
    "Exhausted",
    "Imploded",
    "Melted",
    "Recalibrating",
    "Simplifying",
    "Accepting",
];
pub(crate) const ACT_THINKING: &[&str] = &[
    "Evaluating",
    "Considering",
    "Weighing",
    "Simulating",
    "Modeling",
    "Questioning",
    "Investigating",
    "Dreaming of",
    "Abstracting",
    "Inferring",
    "Meditating on",
    "Hypothesizing",
    "Optimizing",
    "Visualizing",
];

// ── Objects ───────────────────────────────────────────────────────

pub(crate) const OBJ_DEV: &[&str] = &[
    "the build pipeline",
    "memory leaks",
    "all dependencies",
    "the event loop",
    "parallel threads",
    "null references",
    "the type system",
    "edge cases",
    "async chaos",
    "legacy spaghetti",
    "YAML indentation",
    "the production DB",
    "unresolved PRs",
    "the borrow checker",
    "the monad",
    "the linker",
    "clean code",
    "the refactor",
];
pub(crate) const OBJ_SPACE: &[&str] = &[
    "cosmic background noise",
    "the event horizon",
    "orbital parameters",
    "dark matter traces",
    "parallel universes",
    "the observable scope",
    "stellar coordinates",
    "quantum foam",
    "spacetime curvature",
    "void pointers",
    "the flux capacitor",
    "the golden record",
];
pub(crate) const OBJ_SCIENCE: &[&str] = &[
    "entropy levels",
    "wave functions",
    "energy states",
    "the hypothesis",
    "controlled variables",
    "molecular noise",
    "the signal",
    "quantum states",
    "unknown constants",
    "the double-slit experiment",
    "Schrödinger's cat",
];
pub(crate) const OBJ_ABSURD: &[&str] = &[
    "the rubber duck",
    "coffee levels",
    "the cat on keyboard",
    "semicolons",
    "the D20",
    "stack overflow",
    "the intern",
    "the void",
    "common sense",
    "blinker fluid",
    "the 'it works on my machine' seal",
    "the missing bracket",
];
pub(crate) const OBJ_NATURE: &[&str] = &[
    "the root system",
    "fallen branches",
    "the undergrowth",
    "moss patterns",
    "the tree rings",
    "canopy layers",
    "mycelium networks",
    "wind currents",
    "leaf patterns",
    "photosynthetic efficiency",
    "the sap flow",
];
pub(crate) const OBJ_AI: &[&str] = &[
    "the latent space",
    "hallucination filters",
    "token budgets",
    "vector embeddings",
    "stochastic parrots",
    "RLHF feedback",
    "prompt injections",
    "overfitting tendencies",
    "the neural pathways",
];

// ── Twists ────────────────────────────────────────────────────────

pub(crate) const TWIST_FUNNY: &[&str] = &[
    "(probably)",
    "(don't panic)",
    "(it works on my machine)",
    "(send help)",
    "(this is fine)",
    "(might explode)",
    "(no guarantees)",
    "(fingers crossed)",
    "(legacy debt included)",
    "(at least it's not COBOL)",
    "(O(n!) efficiency)",
    "(it's a feature now)",
    "(sponsored by caffeine)",
    "(it was DNS)",
    "(allegedly)",
    "(standard procedure)",
    "(error 404: joke not found)",
    "(oops)",
];
pub(crate) const TWIST_POETIC: &[&str] = &[
    "across dimensions",
    "in the void",
    "beyond observable limits",
    "through the event horizon",
    "between the stars",
    "at the edge of reason",
    "in silence",
    "beyond the known",
    "under the canopy",
];
pub(crate) const TWIST_ADVICE: &[&str] = &[
    "— keep it simple",
    "— read the logs",
    "— don't overthink it",
    "— ship small changes",
    "— test before trusting",
    "— name things properly",
    "— fail fast",
    "— question assumptions",
    "— try turning it off and on",
    "— take a deep breath",
    "— it's just code",
];
pub(crate) const TWIST_CHILL: &[&str] = &[
    "smoothly",
    "with patience",
    "calmly",
    "just fine",
    "as intended",
    "all good",
    "no rush",
    "step by step",
    "in harmony",
    "perfectly",
];

// ── Direct phrases (context-driven) ──────────────────────────────

pub(crate) const PH_IDLE: &[&str] = &[
    "the canopy rests",
    "leaves settling",
    "photosynthesis mode",
    "listening to the forest",
    "roots are deep",
    "quiet among the branches",
    "the understory hums",
    "dappled sunlight",
    "garbage collecting dead leaves",
    "waiting for a breeze (or a task)",
    "watching the shadows move",
];
pub(crate) const PH_SPAWN: &[&str] = &[
    "new growth detected",
    "a seedling emerges",
    "branches extending",
    "the forest expands",
    "fresh leaves unfurling",
    "welcome to the grove",
    "git checkout -b new-branch-literally",
    "planting a new seed",
];
pub(crate) const PH_SUCCESS: &[&str] = &[
    "sunlight breaks through",
    "the forest hums",
    "equilibrium restored",
    "another ring in the trunk",
    "the canopy thrives",
    "fruits of labor",
    "100% test coverage (of my leaves)",
    "blooming beautifully",
    "the ecosystem is stable",
];
pub(crate) const PH_ERROR: &[&str] = &[
    "storm damage reported",
    "a branch gave way",
    "the wind picks up",
    "lightning struck nearby",
    "roots need attention",
    "the canopy sways hard",
    "wildfire in the server room",
    "nature finds a way",
    "a leaf fell prematurely",
    "rebalancing the soil",
];
pub(crate) const PH_SCROLL: &[&str] = &[
    "exploring the layers",
    "scanning tree rings",
    "tracing the bark",
    "reading the growth",
    "deeper into the forest",
    "following the grain",
    "grep-ing through the foliage",
];
pub(crate) const PH_BUSY: &[&str] = &[
    "the forest is alive",
    "all branches active",
    "ecosystem in full swing",
    "photosynthesis overload",
    "the canopy buzzes",
    "biodiversity peak",
    "parallel processing chlorophyll",
];

// ── Types ─────────────────────────────────────────────────────────

/// Contextual hint about what the user is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WhimContext {
    Idle,
    AgentSpawned,
    AgentDone,
    AgentFailed,
    TaskRunning,
    Scrolling,
    Busy,
    MissionUnlocked,
}

pub(crate) const PH_MISSION: &[&str] = &[
    "mission unlocked — the canopy remembers",
    "achievement logged in the moss",
    "a new medal for your explorer's sash",
    "the forest applauds quietly",
];
