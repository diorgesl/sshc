//! Emulador de terminal mínimo: grid de células + parser VT (crate `vte`).
//!
//! Cobre o que sessões de rede usam: CR/LF/BS/TAB, cursor (A/B/C/D/H/f/G/d),
//! erase (J/K), SGR 8/16/256/truecolor. Sem scrollback, sem regiões de scroll,
//! sem seleção — de propósito.

use vte::{Params, Perform};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
        }
    }
}

/// Linhas guardadas acima da tela (limite fixo, em linhas).
const MAX_HISTORY: usize = 2000;

pub struct Screen {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    history: std::collections::VecDeque<Vec<Cell>>,
    /// Quantas linhas a visão está acima do fim (0 = ao vivo).
    scroll: usize,
    x: usize,
    y: usize,
    fg: Color,
    bg: Color,
    bold: bool,
    saved: (usize, usize),
    bracketed_paste: bool,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            history: std::collections::VecDeque::new(),
            scroll: 0,
            x: 0,
            y: 0,
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            saved: (0, 0),
            bracketed_paste: false,
        }
    }

    /// O shell pediu `CSI ? 2004 h` (bracketed paste)?
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.x, self.y)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        for row in self.history.iter_mut() {
            row.resize(cols, Cell::default());
        }
        let mut cells = vec![Cell::default(); cols * rows];
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                cells[y * cols + x] = self.cells[y * self.cols + x];
            }
        }
        self.cells = cells;
        self.cols = cols;
        self.rows = rows;
        self.x = self.x.min(cols - 1);
        self.y = self.y.min(rows - 1);
    }

    /// Texto da linha, sem estilos (útil para busca/testes).
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.rows {
            return String::new();
        }
        (0..self.cols)
            .map(|x| self.cells[row * self.cols + x].ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    fn newline(&mut self) {
        if self.y + 1 >= self.rows {
            self.scroll_up(1);
            self.y = self.rows - 1;
        } else {
            self.y += 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.rows);
        for _ in 0..n {
            let row: Vec<Cell> = self.cells.drain(0..self.cols).collect();
            self.history.push_back(row);
        }
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
        self.cells
            .extend(std::iter::repeat(Cell::default()).take(n * self.cols));
        // Mantém a visão na mesma linha absoluta quando já está rolando.
        if self.scroll > 0 {
            self.scroll = (self.scroll + n).min(self.history.len());
        }
    }

    // --- scrollback -------------------------------------------------------

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Total de linhas: histórico + tela.
    pub fn total_rows(&self) -> usize {
        self.history.len() + self.rows
    }

    /// Quanto a visão está acima do fim (0 = ao vivo).
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    pub fn scroll_back(&mut self, lines: usize) {
        self.scroll = (self.scroll + lines).min(self.history.len());
    }

    pub fn scroll_forward(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn scroll_reset(&mut self) {
        self.scroll = 0;
    }

    /// Deixa a linha absoluta visível (usado pela busca).
    pub fn scroll_to_row(&mut self, row: usize) {
        let top_wanted = row.saturating_sub(self.rows / 2).min(self.history.len());
        self.scroll = self.history.len() - top_wanted;
    }

    /// Linha absoluta (0 = primeira do histórico).
    pub fn absolute_row(&self, index: usize) -> &[Cell] {
        if index < self.history.len() {
            &self.history[index]
        } else {
            let row = index - self.history.len();
            let start = row * self.cols;
            &self.cells[start..start + self.cols]
        }
    }

    /// Linha visível na tela (0 = topo do que aparece agora).
    pub fn visible_row(&self, index: usize) -> &[Cell] {
        let top = self
            .total_rows()
            .saturating_sub(self.rows)
            .saturating_sub(self.scroll);
        self.absolute_row(top + index)
    }

    /// Texto de uma linha absoluta, sem estilos.
    pub fn absolute_row_text(&self, index: usize) -> String {
        self.absolute_row(index)
            .iter()
            .map(|cell| cell.ch)
            .collect()
    }

    fn erase_line(&mut self, mode: u16) {
        let (start, end) = match mode {
            0 => (self.x, self.cols),
            1 => (0, self.x + 1),
            _ => (0, self.cols),
        };
        for x in start..end {
            let i = self.idx(x, self.y);
            self.cells[i] = Cell {
                bg: self.bg,
                ..Cell::default()
            };
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                for x in self.x..self.cols {
                    let i = self.idx(x, self.y);
                    self.cells[i] = Cell::default();
                }
                for y in self.y + 1..self.rows {
                    for x in 0..self.cols {
                        let i = self.idx(x, y);
                        self.cells[i] = Cell::default();
                    }
                }
            }
            1 => {
                for y in 0..self.y {
                    for x in 0..self.cols {
                        let i = self.idx(x, y);
                        self.cells[i] = Cell::default();
                    }
                }
                for x in 0..=self.x.min(self.cols - 1) {
                    let i = self.idx(x, self.y);
                    self.cells[i] = Cell::default();
                }
            }
            _ => {
                self.cells.iter_mut().for_each(|c| *c = Cell::default());
            }
        }
    }

    fn sgr(&mut self, params: &Params) {
        let values: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();
        let mut i = 0;
        if values.is_empty() {
            self.reset_style();
            return;
        }
        while i < values.len() {
            match values[i] {
                0 => self.reset_style(),
                1 => self.bold = true,
                22 => self.bold = false,
                30..=37 => self.fg = Color::Idx((values[i] - 30) as u8),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Idx((values[i] - 40 + 8) as u8),
                49 => self.bg = Color::Default,
                90..=97 => self.fg = Color::Idx((values[i] - 90 + 8) as u8),
                100..=107 => self.bg = Color::Idx((values[i] - 100 + 16) as u8),
                38 | 48 => {
                    let target_fg = values[i] == 38;
                    match values.get(i + 1).copied() {
                        Some(5) => {
                            if let Some(n) = values.get(i + 2).copied() {
                                let color = Color::Idx(n as u8);
                                if target_fg {
                                    self.fg = color
                                } else {
                                    self.bg = color
                                }
                                i += 2;
                            }
                        }
                        Some(2) => {
                            if let (Some(r), Some(g), Some(b)) = (
                                values.get(i + 2).copied(),
                                values.get(i + 3).copied(),
                                values.get(i + 4).copied(),
                            ) {
                                let color = Color::Rgb(r as u8, g as u8, b as u8);
                                if target_fg {
                                    self.fg = color
                                } else {
                                    self.bg = color
                                }
                                i += 4;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_style(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.bold = false;
    }

    fn put(&mut self, ch: char) {
        if self.x >= self.cols {
            self.x = 0;
            self.newline();
        }
        let i = self.idx(self.x, self.y);
        self.cells[i] = Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
        };
        self.x += 1;
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.x = self.x.saturating_sub(1),
            0x09 => {
                let next = ((self.x / 8) + 1) * 8;
                self.x = next.min(self.cols - 1);
            }
            0x0a..=0x0c => self.newline(),
            0x0d => self.x = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _inter: &[u8], _ignore: bool, action: char) {
        let n = |i: usize| {
            params
                .iter()
                .nth(i)
                .and_then(|p| p.first().copied())
                .filter(|v| *v > 0)
        };
        match action {
            'A' => self.y = self.y.saturating_sub(n(0).unwrap_or(1) as usize),
            'B' => self.y = (self.y + n(0).unwrap_or(1) as usize).min(self.rows - 1),
            'C' => self.x = (self.x + n(0).unwrap_or(1) as usize).min(self.cols - 1),
            'D' => self.x = self.x.saturating_sub(n(0).unwrap_or(1) as usize),
            'G' => self.x = (n(0).unwrap_or(1) as usize - 1).min(self.cols - 1),
            'd' => self.y = (n(0).unwrap_or(1) as usize - 1).min(self.rows - 1),
            'H' | 'f' => {
                let row = n(0).unwrap_or(1) as usize;
                let col = n(1).unwrap_or(1) as usize;
                self.y = (row - 1).min(self.rows - 1);
                self.x = (col - 1).min(self.cols - 1);
            }
            'h' | 'l' if _inter.contains(&b'?') => {
                let enable = action == 'h';
                for param in params.iter() {
                    if param.first() == Some(&2004) {
                        self.bracketed_paste = enable;
                    }
                }
            }
            'J' => self.erase_display(
                params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0),
            ),
            'K' => self.erase_line(
                params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0),
            ),
            'm' => self.sgr(params),
            's' => self.saved = (self.x, self.y),
            'u' => {
                self.x = self.saved.0.min(self.cols - 1);
                self.y = self.saved.1.min(self.rows - 1);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _inter: &[u8], _ignore: bool, byte: u8) {
        if byte == b'c' {
            self.cells.iter_mut().for_each(|c| *c = Cell::default());
            self.x = 0;
            self.y = 0;
            self.reset_style();
        }
    }
}

/// Parser + tela. O `Parser` guarda estado de escape entre chunks.
pub struct Terminal {
    parser: vte::Parser,
    screen: Screen,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: Screen::new(cols, rows),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(term: &mut Terminal, text: &str) {
        term.feed(text.as_bytes());
    }

    #[test]
    fn prints_and_wraps() {
        let mut t = Terminal::new(5, 2);
        feed(&mut t, "abcdefg");
        assert_eq!(t.screen().row_text(0), "abcde");
        assert_eq!(t.screen().row_text(1), "fg");
    }

    #[test]
    fn newline_at_bottom_scrolls() {
        let mut t = Terminal::new(8, 2);
        feed(&mut t, "um\r\ndois\r\ntres");
        assert_eq!(t.screen().row_text(0), "dois");
        assert_eq!(t.screen().row_text(1), "tres");
    }

    #[test]
    fn cursor_position_and_clear() {
        let mut t = Terminal::new(10, 3);
        feed(&mut t, "\x1b[2;3Hx");
        assert_eq!(t.screen().row_text(1), "  x");
        feed(&mut t, "\x1b[2J");
        assert_eq!(t.screen().row_text(1), "");
        assert_eq!(t.screen().cursor(), (3, 1));
    }

    #[test]
    fn sgr_sets_colors() {
        let mut t = Terminal::new(10, 1);
        feed(&mut t, "\x1b[31ma\x1b[0mb");
        let cells = t.screen().cells();
        assert_eq!(cells[0].fg, Color::Idx(1));
        assert_eq!(cells[0].ch, 'a');
        assert_eq!(cells[1].fg, Color::Default);
    }

    #[test]
    fn truecolor_and_256() {
        let mut t = Terminal::new(4, 1);
        feed(&mut t, "\x1b[38;5;196mX\x1b[38;2;1;2;3mY");
        let cells = t.screen().cells();
        assert_eq!(cells[0].fg, Color::Idx(196));
        assert_eq!(cells[1].fg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn erase_line_modes() {
        let mut t = Terminal::new(5, 1);
        feed(&mut t, "abcde\x1b[3G\x1b[1K");
        assert_eq!(t.screen().row_text(0), "   de");
    }

    #[test]
    fn bracketed_paste_mode_follows_csi() {
        let mut t = Terminal::new(10, 2);
        assert!(!t.screen().bracketed_paste());
        feed(&mut t, "\x1b[?2004h");
        assert!(t.screen().bracketed_paste());
        feed(&mut t, "\x1b[?2004l");
        assert!(!t.screen().bracketed_paste());
    }

    fn row_text(cells: &[Cell]) -> String {
        cells
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn scrollback_keeps_lines_above_screen() {
        let mut t = Terminal::new(8, 2);
        feed(&mut t, "a\r\nb\r\nc");
        assert_eq!(t.screen().history_len(), 1);
        assert_eq!(row_text(t.screen().visible_row(0)), "b");
        assert_eq!(row_text(t.screen().visible_row(1)), "c");

        t.screen.scroll_back(1);
        assert_eq!(t.screen().scroll_offset(), 1);
        assert_eq!(row_text(t.screen().visible_row(0)), "a");
        assert_eq!(row_text(t.screen().visible_row(1)), "b");

        t.screen.scroll_reset();
        assert_eq!(row_text(t.screen().visible_row(0)), "b");
    }

    #[test]
    fn new_output_keeps_the_view_in_place_while_scrolled() {
        let mut t = Terminal::new(8, 2);
        feed(&mut t, "a\r\nb\r\nc");
        t.screen.scroll_back(1);
        let before = row_text(t.screen().visible_row(0)).to_string();

        feed(&mut t, "\r\nd\r\ne");
        assert_eq!(
            row_text(t.screen().visible_row(0)),
            before,
            "linha visível não pode pular"
        );
        assert_eq!(t.screen().scroll_offset(), 3);
    }

    #[test]
    fn scroll_to_row_centers_the_line() {
        let mut t = Terminal::new(8, 3);
        for i in 0..20 {
            feed(&mut t, &format!("linha{i}\r\n"));
        }
        let target = 3;
        t.screen.scroll_to_row(target);
        let visible = (0..3)
            .map(|i| row_text(t.screen().visible_row(i)))
            .collect::<Vec<_>>();
        assert!(visible.iter().any(|line| line == "linha3"), "{visible:?}");
    }

    #[test]
    fn resize_keeps_content() {
        let mut t = Terminal::new(5, 2);
        feed(&mut t, "ab");
        t.resize(10, 3);
        assert_eq!(t.screen().row_text(0), "ab");
        assert_eq!(t.screen().size(), (10, 3));
    }
}
