//! The post-process uniform block: its Rust layout and the member table
//! the GLSL and WGSL preludes are generated from.
//!
//! One byte layout serves both languages. GLSL declares the block
//! `std140`; WGSL's uniform address-space rules give the same offsets for
//! these member types (`vec3` aligns to 16 and a scalar may follow it in
//! the same 16 bytes, array elements stride 16). A test parses both
//! preludes and checks naga's offsets against [`PostUniforms`].
//!
//! `iChannelTime` is a `vec4`, not Ghostty's `float[4]`: a `float[4]` in
//! std140 has a 16-byte stride that WGSL can't express, so the module
//! wouldn't survive the GLSL → WGSL step. Indexing (`iChannelTime[i]`)
//! reads the same either way.

use bytemuck::{Pod, Zeroable};

/// Seconds-valued uniforms use this for "never happened" (`iTimeBell`,
/// `iTimeCursorChange`, unused `iBells` slots).
pub(crate) const NEVER: f32 = -1e6;

/// The uniform block, as uploaded. `vec3` members are stored as
/// `[f32; 4]` (the fourth float is std140 padding).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct PostUniforms {
    pub i_resolution: [f32; 3],
    pub i_time: f32,
    pub i_mouse: [f32; 4],
    pub i_date: [f32; 4],
    pub i_channel_time: [f32; 4],
    pub i_channel_resolution: [[f32; 4]; 4],
    pub i_current_cursor: [f32; 4],
    pub i_previous_cursor: [f32; 4],
    pub i_current_cursor_color: [f32; 4],
    pub i_previous_cursor_color: [f32; 4],
    pub i_bell_cursor: [f32; 4],
    pub i_bells: [[f32; 4]; 4],
    pub i_grid_rect: [f32; 4],
    pub i_cell_size: [f32; 2],
    pub i_time_delta: f32,
    pub i_frame_rate: f32,
    pub i_frame: i32,
    pub i_sample_rate: f32,
    pub i_current_cursor_style: i32,
    pub i_previous_cursor_style: i32,
    pub i_cursor_visible: i32,
    pub i_time_cursor_change: f32,
    pub i_time_focus: f32,
    pub i_focus: i32,
    pub i_time_bell: f32,
    pub i_bell_count: i32,
    pub _pad0: [u32; 2],
    pub i_background_color: [f32; 4],
    pub i_foreground_color: [f32; 4],
    pub i_cursor_color: [f32; 4],
    pub i_cursor_text: [f32; 4],
    pub i_selection_background_color: [f32; 4],
    pub i_selection_foreground_color: [f32; 4],
    pub i_palette: [[f32; 4]; 256],
}

/// Byte offset of `iPalette`. Everything before it is rewritten every
/// post-processed frame; the palette only when it changes.
pub(crate) const PALETTE_OFFSET: usize = core::mem::offset_of!(PostUniforms, i_palette);

impl Default for PostUniforms {
    fn default() -> Self {
        let mut u = Self::zeroed();
        u.i_time_cursor_change = NEVER;
        u.i_time_bell = NEVER;
        u.i_bells = [EMPTY_BELL; 4];
        u
    }
}

/// An unused `iBells` slot: at the origin, rung "never", infinitely long
/// ago.
pub(crate) const EMPTY_BELL: [f32; 4] = [0.0, 0.0, NEVER, 1e6];

/// One uniform-block member: its name (same in both languages), its
/// GLSL type and array suffix, its WGSL type, and its offset in
/// [`PostUniforms`]. Index in [`MEMBERS`] = member index in the naga
/// struct.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Member {
    pub name: &'static str,
    pub glsl: &'static str,
    pub glsl_array: &'static str,
    pub wgsl: &'static str,
    /// Only read by the layout test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub offset: usize,
}

macro_rules! members {
    ($( $name:literal $glsl:literal $arr:literal $wgsl:literal => $field:ident ),* $(,)?) => {
        &[$( Member {
            name: $name,
            glsl: $glsl,
            glsl_array: $arr,
            wgsl: $wgsl,
            offset: core::mem::offset_of!(PostUniforms, $field),
        } ),*]
    };
}

/// The uniform block, in declaration order.
pub(crate) const MEMBERS: &[Member] = members![
    "iResolution" "vec3" "" "vec3f" => i_resolution,
    "iTime" "float" "" "f32" => i_time,
    "iMouse" "vec4" "" "vec4f" => i_mouse,
    "iDate" "vec4" "" "vec4f" => i_date,
    "iChannelTime" "vec4" "" "vec4f" => i_channel_time,
    "iChannelResolution" "vec3" "[4]" "array<vec3f, 4>" => i_channel_resolution,
    "iCurrentCursor" "vec4" "" "vec4f" => i_current_cursor,
    "iPreviousCursor" "vec4" "" "vec4f" => i_previous_cursor,
    "iCurrentCursorColor" "vec4" "" "vec4f" => i_current_cursor_color,
    "iPreviousCursorColor" "vec4" "" "vec4f" => i_previous_cursor_color,
    "iBellCursor" "vec4" "" "vec4f" => i_bell_cursor,
    "iBells" "vec4" "[4]" "array<vec4f, 4>" => i_bells,
    "iGridRect" "vec4" "" "vec4f" => i_grid_rect,
    "iCellSize" "vec2" "" "vec2f" => i_cell_size,
    "iTimeDelta" "float" "" "f32" => i_time_delta,
    "iFrameRate" "float" "" "f32" => i_frame_rate,
    "iFrame" "int" "" "i32" => i_frame,
    "iSampleRate" "float" "" "f32" => i_sample_rate,
    "iCurrentCursorStyle" "int" "" "i32" => i_current_cursor_style,
    "iPreviousCursorStyle" "int" "" "i32" => i_previous_cursor_style,
    "iCursorVisible" "int" "" "i32" => i_cursor_visible,
    "iTimeCursorChange" "float" "" "f32" => i_time_cursor_change,
    "iTimeFocus" "float" "" "f32" => i_time_focus,
    "iFocus" "int" "" "i32" => i_focus,
    "iTimeBell" "float" "" "f32" => i_time_bell,
    "iBellCount" "int" "" "i32" => i_bell_count,
    "iBackgroundColor" "vec3" "" "vec3f" => i_background_color,
    "iForegroundColor" "vec3" "" "vec3f" => i_foreground_color,
    "iCursorColor" "vec3" "" "vec3f" => i_cursor_color,
    "iCursorText" "vec3" "" "vec3f" => i_cursor_text,
    "iSelectionBackgroundColor" "vec3" "" "vec3f" => i_selection_background_color,
    "iSelectionForegroundColor" "vec3" "" "vec3f" => i_selection_foreground_color,
    "iPalette" "vec3" "[256]" "array<vec3f, 256>" => i_palette,
];

/// Bit mask over [`MEMBERS`] indexes for the named members.
pub(crate) fn mask_of(names: &[&str]) -> u64 {
    MEMBERS
        .iter()
        .enumerate()
        .filter(|(_, m)| names.contains(&m.name))
        .fold(0, |acc, (i, _)| acc | (1u64 << i))
}

/// Uniforms that move with wall-clock time alone.
pub(crate) fn time_mask() -> u64 {
    mask_of(&["iTime", "iTimeDelta", "iFrame", "iDate", "iBells"])
}

/// `iDate` (the wall clock is only read for shaders that use it).
pub(crate) fn date_mask() -> u64 {
    mask_of(&["iDate"])
}

/// `iMouse`.
pub(crate) fn mouse_mask() -> u64 {
    mask_of(&["iMouse"])
}

/// Uniforms that change when the window gains or loses focus.
pub(crate) fn focus_mask() -> u64 {
    mask_of(&["iFocus", "iTimeFocus"])
}

/// Uniforms that change on a BEL.
pub(crate) fn bell_mask() -> u64 {
    mask_of(&["iTimeBell", "iBellCursor", "iBellCount", "iBells"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_fit_the_mask() {
        assert!(MEMBERS.len() <= 64);
    }

    #[test]
    fn block_size_is_a_multiple_of_16() {
        assert_eq!(size_of::<PostUniforms>() % 16, 0);
        assert_eq!(PALETTE_OFFSET % 16, 0);
        assert_eq!(size_of::<PostUniforms>(), PALETTE_OFFSET + 256 * 16);
    }

    #[test]
    fn masks_name_real_members() {
        assert_eq!(time_mask().count_ones(), 5);
        assert_eq!(mouse_mask().count_ones(), 1);
        assert_eq!(focus_mask().count_ones(), 2);
        assert_eq!(bell_mask().count_ones(), 4);
    }
}
