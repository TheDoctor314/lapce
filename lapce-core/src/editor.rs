use itertools::Itertools;
use xi_rope::RopeDelta;

use crate::{
    buffer::{Buffer, InvalLines},
    command::EditCommand,
    cursor::{Cursor, CursorMode},
    mode::{Mode, VisualMode},
    movement::Movement,
    register::RegisterData,
    selection::{InsertDrift, SelRegion, Selection},
    syntax::{matching_char, matching_pair_direction, Syntax},
    word::{get_word_property, WordProperty},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditType {
    Other,
    InsertChars,
    InsertNewline,
    Delete,
    Undo,
    Redo,
}

impl EditType {
    /// Checks whether a new undo group should be created between two edits.
    pub fn breaks_undo_group(self, previous: EditType) -> bool {
        self == EditType::Other || self != previous
    }
}

pub struct Editor {}

impl Editor {
    pub fn insert(
        cursor: &mut Cursor,
        buffer: &mut Buffer,
        s: &str,
        syntax: Option<&Syntax>,
    ) {
        if let CursorMode::Insert(selection) = &cursor.mode {
            if s.chars().count() != 1 {
                let (delta, _) =
                    buffer.edit(&[(selection, s)], EditType::InsertChars);
                let selection =
                    selection.apply_delta(&delta, true, InsertDrift::Default);
                cursor.mode = CursorMode::Insert(selection);
            } else {
                let c = s.chars().next().unwrap();
                let matching_pair_type = matching_pair_direction(c);

                // The main edit operations
                let mut edits = vec![];

                // "Late edits" - characters to be inserted after particular regions
                let mut edits_after = vec![];

                let mut selection = selection.clone();
                for (idx, region) in selection.regions_mut().iter_mut().enumerate() {
                    let offset = region.end;
                    let cursor_char = buffer.char_at_offset(offset);

                    if matching_pair_type == Some(false) {
                        if cursor_char == Some(c) {
                            // Skip the closing character
                            let new_offset =
                                buffer.next_grapheme_offset(offset, 1, buffer.len());

                            *region = SelRegion::caret(new_offset);
                            continue;
                        }

                        let line = buffer.line_of_offset(offset);
                        let line_start = buffer.offset_of_line(line);
                        if buffer.slice_to_cow(line_start..offset).trim() == "" {
                            let opening_character = matching_char(c).unwrap();
                            if let Some(previous_offset) = buffer.previous_unmatched(
                                syntax,
                                opening_character,
                                offset,
                            ) {
                                // Auto-indent closing character to the same level as the opening.
                                let previous_line =
                                    buffer.line_of_offset(previous_offset);
                                let line_indent =
                                    buffer.indent_on_line(previous_line);

                                let current_selection =
                                    Selection::region(line_start, offset);

                                edits.push((
                                    current_selection,
                                    format!("{line_indent}{c}"),
                                ));
                                continue;
                            }
                        }
                    }

                    if matching_pair_type == Some(true) {
                        // Create a late edit to insert the closing pair, if allowed.
                        let is_whitespace_or_punct = cursor_char
                            .map(|c| {
                                let prop = get_word_property(c);
                                prop == WordProperty::Lf
                                    || prop == WordProperty::Space
                                    || prop == WordProperty::Punctuation
                            })
                            .unwrap_or(true);

                        if is_whitespace_or_punct {
                            let insert_after = matching_char(c).unwrap();
                            edits_after.push((idx, insert_after));
                        }
                    };

                    let current_selection =
                        Selection::region(region.start, region.end);

                    edits.push((current_selection, c.to_string()));
                }

                // Apply edits to current selection
                let edits = edits
                    .iter()
                    .map(|(selection, content)| (selection, content.as_str()))
                    .collect::<Vec<_>>();

                let (delta, _) = buffer.edit(&edits, EditType::InsertChars);

                // Update selection
                let mut selection =
                    selection.apply_delta(&delta, true, InsertDrift::Default);

                // Apply late edits
                let edits_after = edits_after
                    .iter()
                    .map(|(idx, content)| {
                        let region = &selection.regions()[*idx];
                        (
                            Selection::region(region.start, region.end),
                            content.to_string(),
                        )
                    })
                    .collect::<Vec<_>>();

                let edits_after = edits_after
                    .iter()
                    .map(|(selection, content)| (selection, content.as_str()))
                    .collect::<Vec<_>>();

                buffer.edit(&edits_after, EditType::InsertChars);

                // Adjust selection according to previous late edits
                let mut adjustment = 0;
                for region in selection.regions_mut().iter_mut().sorted_by(
                    |region_a, region_b| region_a.start().cmp(&region_b.start()),
                ) {
                    *region = SelRegion::new(
                        region.start + adjustment,
                        region.end + adjustment,
                        None,
                    );

                    if let Some(inserted) =
                        edits_after.iter().find_map(|(selection, str)| {
                            if selection.last_inserted().map(|r| r.start())
                                == Some(region.start())
                            {
                                Some(str)
                            } else {
                                None
                            }
                        })
                    {
                        adjustment += inserted.len();
                    }
                }

                cursor.mode = CursorMode::Insert(selection);
            }
        }
    }

    fn toggle_visual(cursor: &mut Cursor, visual_mode: VisualMode, modal: bool) {
        if !modal {
            return;
        }

        match &cursor.mode {
            CursorMode::Visual { start, end, mode } => {
                if mode != &visual_mode {
                    cursor.mode = CursorMode::Visual {
                        start: *start,
                        end: *end,
                        mode: visual_mode,
                    };
                } else {
                    cursor.mode = CursorMode::Normal(*end);
                };
            }
            _ => {
                let offset = cursor.offset();
                cursor.mode = CursorMode::Visual {
                    start: offset,
                    end: offset,
                    mode: visual_mode,
                };
            }
        }
    }

    pub fn do_paste(
        cursor: &mut Cursor,
        buffer: &mut Buffer,
        data: &RegisterData,
    ) -> Vec<(RopeDelta, InvalLines)> {
        let mut deltas = Vec::new();
        match data.mode {
            VisualMode::Normal => {
                let selection = match cursor.mode {
                    CursorMode::Normal(offset) => {
                        let line_end = buffer.offset_line_end(offset, true);
                        let offset = (offset + 1).min(line_end);
                        Selection::caret(offset)
                    }
                    CursorMode::Insert { .. } | CursorMode::Visual { .. } => {
                        cursor.edit_selection(buffer)
                    }
                };
                let after = cursor.is_insert() || !data.content.contains('\n');
                let (delta, inval_lines) = buffer
                    .edit(&[(&selection, &data.content)], EditType::InsertChars);
                let selection =
                    selection.apply_delta(&delta, after, InsertDrift::Default);
                deltas.push((delta, inval_lines));
                if !after {
                    cursor.update_selection(buffer, selection);
                } else {
                    match cursor.mode {
                        CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                            let offset = buffer.prev_grapheme_offset(
                                selection.min_offset(),
                                1,
                                0,
                            );
                            cursor.mode = CursorMode::Normal(offset);
                        }
                        CursorMode::Insert { .. } => {
                            cursor.mode = CursorMode::Insert(selection);
                        }
                    }
                }
            }
            VisualMode::Linewise | VisualMode::Blockwise => {
                let (selection, content) = match &cursor.mode {
                    CursorMode::Normal(offset) => {
                        let line = buffer.line_of_offset(*offset);
                        let offset = buffer.offset_of_line(line + 1);
                        (Selection::caret(offset), data.content.clone())
                    }
                    CursorMode::Insert(selection) => {
                        let mut selection = selection.clone();
                        for region in selection.regions_mut() {
                            if region.is_caret() {
                                let line = buffer.line_of_offset(region.start);
                                let start = buffer.offset_of_line(line);
                                region.start = start;
                                region.end = start;
                            }
                        }
                        (selection, data.content.clone())
                    }
                    CursorMode::Visual { mode, .. } => {
                        let selection = cursor.edit_selection(buffer);
                        let data = match mode {
                            VisualMode::Linewise => data.content.clone(),
                            _ => "\n".to_string() + &data.content,
                        };
                        (selection, data)
                    }
                };
                let (delta, inval_lines) =
                    buffer.edit(&[(&selection, &content)], EditType::InsertChars);
                let selection = selection.apply_delta(
                    &delta,
                    cursor.is_insert(),
                    InsertDrift::Default,
                );
                deltas.push((delta, inval_lines));
                match cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        let offset = selection.min_offset();
                        let offset = if cursor.is_visual() {
                            offset + 1
                        } else {
                            offset
                        };
                        let line = buffer.line_of_offset(offset);
                        let offset = buffer.first_non_blank_character_on_line(line);
                        cursor.mode = CursorMode::Normal(offset);
                    }
                    CursorMode::Insert(_) => {
                        cursor.mode = CursorMode::Insert(selection);
                    }
                }
            }
        }
        deltas
    }

    pub fn do_edit(
        cursor: &mut Cursor,
        buffer: &mut Buffer,
        cmd: &EditCommand,
        syntax: Option<&Syntax>,
        modal: bool,
    ) -> Vec<(RopeDelta, InvalLines)> {
        let mut deltas = Vec::new();
        use crate::command::EditCommand::*;
        match cmd {
            MoveLineUp => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    for region in selection.regions_mut() {
                        let start_line = buffer.line_of_offset(region.min());
                        if start_line > 0 {
                            let previous_line_len =
                                buffer.line_content(start_line - 1).len();

                            let end_line = buffer.line_of_offset(region.max());
                            let start = buffer.offset_of_line(start_line);
                            let end = buffer.offset_of_line(end_line + 1);
                            let content =
                                buffer.slice_to_cow(start..end).to_string();
                            let (delta, inval_lines) = buffer.edit(
                                &[
                                    (&Selection::region(start, end), ""),
                                    (
                                        &Selection::caret(
                                            buffer.offset_of_line(start_line - 1),
                                        ),
                                        &content,
                                    ),
                                ],
                                EditType::InsertChars,
                            );
                            deltas.push((delta, inval_lines));
                            region.start -= previous_line_len;
                            region.end -= previous_line_len;
                        }
                    }
                    cursor.mode = CursorMode::Insert(selection);
                }
            }
            NormalMode => {
                if !modal {
                    if let CursorMode::Insert(selection) = &cursor.mode {
                        match selection.regions().len() {
                            i if i > 1 => {
                                if let Some(region) = selection.last_inserted() {
                                    let new_selection =
                                        Selection::region(region.start, region.end);
                                    cursor.mode = CursorMode::Insert(new_selection);
                                    return deltas;
                                }
                            }
                            i if i == 1 => {
                                let region = selection.regions()[0];
                                if !region.is_caret() {
                                    let new_selection = Selection::caret(region.end);
                                    cursor.mode = CursorMode::Insert(new_selection);
                                    return deltas;
                                }
                            }
                            _ => (),
                        }
                    }

                    return deltas;
                }

                let offset = match &cursor.mode {
                    CursorMode::Insert(selection) => {
                        let offset = selection.min_offset();
                        buffer.prev_grapheme_offset(
                            offset,
                            1,
                            buffer.offset_of_line(buffer.line_of_offset(offset)),
                        )
                    }
                    CursorMode::Visual { end, .. } => {
                        buffer.offset_line_end(*end, false).min(*end)
                    }
                    CursorMode::Normal(offset) => *offset,
                };

                cursor.mode = CursorMode::Normal(offset);
                cursor.horiz = None;
            }
            InsertMode => {
                cursor.mode = CursorMode::Insert(Selection::caret(cursor.offset()));
            }
            ToggleVisualMode => {
                Self::toggle_visual(cursor, VisualMode::Normal, modal);
            }
            ToggleLinewiseVisualMode => {
                Self::toggle_visual(cursor, VisualMode::Linewise, modal);
            }
            ToggleBlockwiseVisualMode => {
                Self::toggle_visual(cursor, VisualMode::Blockwise, modal);
            }
        }
        deltas
    }
}

#[cfg(test)]
mod test {
    use crate::buffer::Buffer;
    use crate::cursor::{Cursor, CursorMode};
    use crate::editor::Editor;
    use crate::selection::{SelRegion, Selection};

    #[test]
    fn test_insert_simple() {
        let mut buffer = Buffer::new("abc");
        let mut cursor = Cursor::new(CursorMode::Insert(Selection::caret(1)), None);

        Editor::insert(&mut cursor, &mut buffer, "e", None);
        assert_eq!("aebc", buffer.slice_to_cow(0..buffer.len()));
    }

    #[test]
    fn test_insert_multiple_cursor() {
        let mut buffer = Buffer::new("abc\nefg\n");
        let mut selection = Selection::new();
        selection.add_region(SelRegion::caret(1));
        selection.add_region(SelRegion::caret(5));
        let mut cursor = Cursor::new(CursorMode::Insert(selection), None);

        Editor::insert(&mut cursor, &mut buffer, "i", None);
        assert_eq!("aibc\neifg\n", buffer.slice_to_cow(0..buffer.len()));
    }

    #[test]
    fn test_insert_complex() {
        let mut buffer = Buffer::new("abc\nefg\n");
        let mut selection = Selection::new();
        selection.add_region(SelRegion::caret(1));
        selection.add_region(SelRegion::caret(5));
        let mut cursor = Cursor::new(CursorMode::Insert(selection), None);

        Editor::insert(&mut cursor, &mut buffer, "i", None);
        assert_eq!("aibc\neifg\n", buffer.slice_to_cow(0..buffer.len()));
        Editor::insert(&mut cursor, &mut buffer, "j", None);
        assert_eq!("aijbc\neijfg\n", buffer.slice_to_cow(0..buffer.len()));
        Editor::insert(&mut cursor, &mut buffer, "{", None);
        assert_eq!("aij{bc\neij{fg\n", buffer.slice_to_cow(0..buffer.len()));
        Editor::insert(&mut cursor, &mut buffer, " ", None);
        assert_eq!("aij{ bc\neij{ fg\n", buffer.slice_to_cow(0..buffer.len()));
    }

    #[test]
    fn test_insert_pair() {
        let mut buffer = Buffer::new("a bc\ne fg\n");
        let mut selection = Selection::new();
        selection.add_region(SelRegion::caret(1));
        selection.add_region(SelRegion::caret(6));
        let mut cursor = Cursor::new(CursorMode::Insert(selection), None);

        Editor::insert(&mut cursor, &mut buffer, "{", None);
        assert_eq!("a{} bc\ne{} fg\n", buffer.slice_to_cow(0..buffer.len()));
        Editor::insert(&mut cursor, &mut buffer, "}", None);
        assert_eq!("a{} bc\ne{} fg\n", buffer.slice_to_cow(0..buffer.len()));
    }
}
