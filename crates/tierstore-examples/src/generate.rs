//! The `[[generate]]` tables of an example: files made from a template of the example, one for
//! each n of 1 to `count`, with `${n}` in the template and in the path replaced by n. The
//! template knows no other value of the run, so what it makes is the same in every run.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use crate::error::{Error, invalid_at, io_at};
use crate::spec::{Generate, SPEC_FILE, expand};

/// Writes the files of `generates` under `work`, from the templates in `example`.
///
/// # Errors
/// A template cannot be read, it or a path names a value other than `${n}`, or a file cannot be
/// written or is there already (two generators that write one path).
pub fn generate(example: &Path, work: &Path, generates: &[Generate]) -> Result<(), Error> {
    let spec = example.join(SPEC_FILE);
    for each in generates {
        let template_path = example.join(&each.template);
        let template = fs::read_to_string(&template_path).map_err(io_at(&template_path))?;
        for n in 1..=each.count {
            let vars = BTreeMap::from([("n".to_owned(), n.to_string())]);
            let relative = expand(&each.path, &vars).map_err(invalid_at(&spec))?;
            let text = expand(&template, &vars).map_err(invalid_at(&template_path))?;
            let path = work.join(relative);
            let dir = path.parent().unwrap_or(work);
            fs::create_dir_all(dir).map_err(io_at(dir))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(io_at(&path))?;
            file.write_all(text.as_bytes()).map_err(io_at(&path))?;
        }
    }
    Ok(())
}
