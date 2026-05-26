//! Shared table-printing utilities using comfy-table.
//!
//! All CLI table output should use [`CliTable`] to ensure consistent formatting
//! (preset, alignment, divider style) across all commands.

use comfy_table::{CellAlignment, ContentArrangement, Table, presets};

/// Column alignment.
#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Right,
}

/// A column definition for CLI table output.
pub struct Col {
    pub header: &'static str,
    pub align: Align,
}

/// Builder for printing a consistently-formatted CLI table.
///
/// Uses the `NOTHING` preset (no borders) with `-` dividers above and below
/// the header, and optionally before a total/summary row.
pub struct CliTable {
    columns: Vec<Col>,
    rows: Vec<Vec<String>>,
    total_row: Option<Vec<String>>,
    title: Option<String>,
}

impl CliTable {
    pub fn new(columns: Vec<Col>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            total_row: None,
            title: None,
        }
    }

    /// Set a title displayed above the table.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn row(mut self, cells: Vec<String>) -> Self {
        self.rows.push(cells);
        self
    }

    pub fn rows(mut self, rows: Vec<Vec<String>>) -> Self {
        self.rows = rows;
        self
    }

    pub fn total_row(mut self, cells: Vec<String>) -> Self {
        self.total_row = Some(cells);
        self
    }

    /// Render the table into a list of output lines.
    pub fn render(self) -> Vec<String> {
        let mut table = Table::new();
        table.load_preset(presets::NOTHING);
        table.set_content_arrangement(ContentArrangement::Disabled);

        // Set header
        let headers: Vec<&str> = self.columns.iter().map(|c| c.header).collect();
        table.set_header(headers);

        // Configure column alignment
        for (i, col) in self.columns.iter().enumerate() {
            if let Some(column) = table.column_mut(i) {
                match col.align {
                    Align::Left => column.set_cell_alignment(CellAlignment::Left),
                    Align::Right => column.set_cell_alignment(CellAlignment::Right),
                }
            }
        }

        // Add data rows
        for row in &self.rows {
            table.add_row(row);
        }

        // Add total row (to participate in width calculation)
        if let Some(ref total) = self.total_row {
            table.add_row(total);
        }

        // Render all lines
        let lines: Vec<String> = table.lines().collect();
        if lines.is_empty() {
            return Vec::new();
        }

        // Determine divider width from the first rendered line
        let table_width = lines[0].len();
        let divider = "-".repeat(table_width);

        let mut output = Vec::new();

        if let Some(title) = self.title {
            output.push(title);
        }

        // Split lines into header, data, and optional total
        let mut lines = lines.into_iter();
        let header = lines.next().unwrap();

        output.push(divider.clone());
        output.push(header);
        output.push(divider.clone());

        let mut remaining: Vec<String> = lines.collect();

        if self.total_row.is_some() {
            let total = remaining.pop().unwrap();
            output.extend(remaining);
            output.push(divider.clone());
            output.push(total);
        } else {
            output.extend(remaining);
        }

        output.push(divider);
        output
    }

    /// Render and print the table to stdout.
    pub fn print(self) {
        for line in self.render() {
            println!("{line}");
        }
    }
}

/// Format a number with thousands separators (commas).
pub fn format_number(n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(100), "100");
        assert_eq!(format_number(10000000), "10,000,000");
    }

    #[test]
    fn test_cli_table_basic() {
        // Smoke test — just verify it doesn't panic
        CliTable::new(vec![
            Col {
                header: "Name",
                align: Align::Left,
            },
            Col {
                header: "Count",
                align: Align::Right,
            },
        ])
        .row(vec!["hello".into(), "42".into()])
        .print();
    }

    #[test]
    fn test_cli_table_with_total() {
        CliTable::new(vec![
            Col {
                header: "Model",
                align: Align::Left,
            },
            Col {
                header: "Tokens",
                align: Align::Right,
            },
        ])
        .rows(vec![
            vec!["gpt-4".into(), "1,234".into()],
            vec!["claude".into(), "5,678".into()],
        ])
        .total_row(vec!["Total".into(), "6,912".into()])
        .print();
    }
}
