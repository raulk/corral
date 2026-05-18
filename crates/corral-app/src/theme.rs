//! Centralised colour and label tables for agent state and tool kind.
//!
//! Tile and tooltip share these so a palette change is a one-line edit.
//! Brand accents (`tool_accent`) and tile background tints (`tile_bg`)
//! intentionally diverge: tile_bg is a dark ~25% tint used as a fill,
//! tool_accent is the full brand hue used for the tooltip header dot.

use corral_core::agent::Tool;
use corral_core::status::AgentState;

// Tailwind-aligned dot colours per state. `AwaitingUser` deliberately
// uses a saturated pink/fuchsia: it has to read as "you, specifically,
// right now" — distinct from the amber "this turn ended, you may want
// to follow up" of `NeedsInput`.
pub const STATE_ACTIVE: u32 = 0x22c55e; // green-500
pub const STATE_AWAITING_USER: u32 = 0xec4899; // pink-500
pub const STATE_NEEDS_INPUT: u32 = 0xf59e0b; // amber-500
pub const STATE_IDLE: u32 = 0x71717a; // zinc-500
pub const STATE_CLOSED: u32 = 0x3f3f46; // zinc-700

// Subtle brand tints over the dark strip. Computed as ~25% of the brand
// hue blended into the base tile dark so the tile reads as "warm" or
// "cool" at a glance without being loud.
//   Claude  → Anthropic orange  (#D97757)
//   Codex   → indigo/violet     (#7C6FF5)
const TILE_BG_CLAUDE: u32 = 0x4e362d;
const TILE_BG_CLAUDE_HOVER: u32 = 0x5e4136;
const TILE_BG_CODEX: u32 = 0x393660;
const TILE_BG_CODEX_HOVER: u32 = 0x474378;

// Full brand hues for header accents (tooltip and elsewhere).
const TOOL_ACCENT_CLAUDE: u32 = 0xd97757;
const TOOL_ACCENT_CODEX: u32 = 0x8b85f5;

pub fn state_color(state: AgentState) -> u32 {
    match state {
        AgentState::Active => STATE_ACTIVE,
        AgentState::AwaitingUser => STATE_AWAITING_USER,
        AgentState::NeedsInput => STATE_NEEDS_INPUT,
        AgentState::Idle => STATE_IDLE,
        AgentState::Closed => STATE_CLOSED,
    }
}

pub fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Active => "active",
        AgentState::AwaitingUser => "awaiting answer",
        AgentState::NeedsInput => "needs input",
        AgentState::Idle => "idle",
        AgentState::Closed => "closed",
    }
}

/// `(base, hover)` background colours for a tile.
pub fn tile_bg(tool: Tool) -> (u32, u32) {
    match tool {
        Tool::Claude => (TILE_BG_CLAUDE, TILE_BG_CLAUDE_HOVER),
        Tool::CodexCli | Tool::CodexAppServer => (TILE_BG_CODEX, TILE_BG_CODEX_HOVER),
    }
}

/// Full brand hue used for tooltip header accents.
pub fn tool_accent(tool: Tool) -> u32 {
    match tool {
        Tool::Claude => TOOL_ACCENT_CLAUDE,
        Tool::CodexCli | Tool::CodexAppServer => TOOL_ACCENT_CODEX,
    }
}
