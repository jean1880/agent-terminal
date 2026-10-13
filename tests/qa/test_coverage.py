#!/usr/bin/env python3
"""Regression checks that coverage counts production lines only."""

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import coverage

SOURCE = '''fn real() {
    let brace = '{';
    let s = "}}} not code";
    let raw = r#"{"json": {}}"#;
    // } a comment brace
}

#[cfg(test)]
fn helper() -> u8 {
    1
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[allow(dead_code)]
mod inline {
    #[test]
    fn t() { assert_eq!("{", "{"); }
}

fn after() {}
'''


class CoverageTests(unittest.TestCase):
    def test_braces_in_strings_chars_and_comments_are_not_code(self):
        code = coverage.strip_code(SOURCE)
        first = code.split("#[cfg(test)]")[0]
        self.assertEqual(first.count("{"), first.count("}"))
        self.assertEqual(code.count("\n"), SOURCE.count("\n"))

    def test_test_items_and_declared_test_files_are_found(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "view.rs"
            path.write_text(SOURCE)
            lines, files = coverage.test_only(path)
        numbered = SOURCE.splitlines()
        self.assertNotIn(1, lines)
        self.assertIn(numbered.index("fn helper() -> u8 {") + 1, lines)
        self.assertIn(numbered.index("    fn t() { assert_eq!(\"{\", \"{\"); }") + 1, lines)
        self.assertNotIn(numbered.index("fn after() {}") + 1, lines)
        self.assertIn(Path(directory) / "view" / "tests.rs", files)

    def test_production_totals_drop_test_lines_and_test_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            main = root / "src" / "main.rs"
            main.write_text("fn a() {}\nfn b() {}\n#[cfg(test)]\nmod testutil;\n#[cfg(test)]\nmod tests {\n    fn c() {}\n}\n")
            (root / "src" / "testutil.rs").write_text("pub fn helper() {}\n")
            lcov = (f"SF:{main}\nDA:1,3\nDA:2,0\nDA:7,9\nend_of_record\n"
                    f"SF:{root / 'src' / 'testutil.rs'}\nDA:1,0\nend_of_record\n")
            with patch.object(coverage, "ROOT", root):
                totals = coverage.production_lines(lcov)
        self.assertEqual(totals, {"src/main.rs": (1, 2)})


if __name__ == "__main__":
    unittest.main()
