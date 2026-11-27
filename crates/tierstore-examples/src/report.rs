//! The report of a run: the lines its checks gave, beside the example's `expected.txt`.

/// What a run found, and what it was to find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The lines the run gave, each ended by a newline.
    pub actual: String,
    /// The text of `expected.txt`.
    pub expected: String,
    /// The checks that stand for a missing feature and pass now (each a sentence).
    pub now_passing: Vec<String>,
}

impl Report {
    /// Whether the run gave exactly the expected text and no expected failure passes.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.actual == self.expected && self.now_passing.is_empty()
    }

    /// The names (first words) of the lines that differ: in one text and not the other, or with
    /// another value; in the order of the actual lines, then of the expected ones.
    #[must_use]
    pub fn differing(&self) -> Vec<String> {
        let actual = lines(&self.actual);
        let expected = lines(&self.expected);
        let mut names: Vec<String> = Vec::new();
        for (name, _) in actual.iter().chain(&expected) {
            let mine = actual.iter().find(|(n, _)| n == name).map(|(_, l)| *l);
            let theirs = expected.iter().find(|(n, _)| n == name).map(|(_, l)| *l);
            let new = !names.iter().any(|n| n.as_str() == *name);
            if mine != theirs && new {
                names.push((*name).to_owned());
            }
        }
        names
    }

    /// What differs, for a person: each differing line as expected and as found, then the
    /// expected failures that pass.
    #[must_use]
    pub fn describe(&self) -> String {
        let actual = lines(&self.actual);
        let expected = lines(&self.expected);
        let shown = |all: &[(&str, &str)], name: &str| {
            let found = all.iter().find(|(n, _)| *n == name).map(|(_, l)| *l);
            found.unwrap_or("(no line)").to_owned()
        };
        let mut text = String::new();
        for name in self.differing() {
            text.push_str(&format!(
                "- expected: {}\n+ found:    {}\n",
                shown(&expected, &name),
                shown(&actual, &name)
            ));
        }
        for why in &self.now_passing {
            text.push_str(&format!("! {why}\n"));
        }
        text
    }
}

/// Each line of `text` by its first word.
fn lines(text: &str) -> Vec<(&str, &str)> {
    let words = text
        .lines()
        .map(|line| (line.split(' ').next().unwrap_or_default(), line));
    words.collect()
}
