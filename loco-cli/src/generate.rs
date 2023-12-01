use ignore::WalkBuilder;
use ignore::WalkState;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

// Name of generator template that should be existing in each starter folder
const GENERATOR_FILE_NAME: &str = "generator.yaml";

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Represents the configuration of a template generator.
pub struct Template {
    /// Description of the template.
    pub description: String,
    #[serde(with = "serde_regex", skip_serializing)]
    /// List of file patterns that the generator will process.
    pub file_patterns: Option<Vec<Regex>>,
    /// List of rules for placeholder replacement in the generator.
    pub rules: Option<Vec<TemplateRule>>,
}

#[derive(Debug, Clone)]
/// Represents internal placeholders to be replaced.
pub struct ArgsPlaceholder {
    pub lib_name: String,
    pub secret: String,
}

#[derive(Debug, Clone, Serialize)]
/// Enum representing different kinds of template rules.
pub enum TemplateRuleKind {
    LibName,
    Secret,
    Any(String),
}

/// Deserialize [`TemplateRuleKind`] for supporting any string replacements
impl<'de> Deserialize<'de> for TemplateRuleKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: serde_yaml::Value = Deserialize::deserialize(deserializer)?;

        match &value {
            serde_yaml::Value::String(s) => match s.as_str() {
                "LibName" => Ok(Self::LibName),
                "Secret" => Ok(Self::Secret),
                _ => Ok(Self::Any(s.clone())),
            },
            _ => Err(serde::de::Error::custom("Invalid TemplateRuleKind value")),
        }
    }
}

impl TemplateRuleKind {
    #[must_use]
    /// Get the value from the rule Kind.
    pub fn get_val(&self, args: &ArgsPlaceholder) -> String {
        match self {
            Self::LibName => args.lib_name.to_string(),
            Self::Secret => args.secret.to_string(),
            Self::Any(s) => s.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Represents a placeholder replacement rule.
pub struct TemplateRule {
    #[serde(with = "serde_regex")]
    /// Pattern to search in the file
    pub pattern: Regex,
    /// The replacement kind
    pub kind: TemplateRuleKind,
    #[serde(with = "serde_regex", skip_serializing)]
    /// List of template generator rule for replacement
    pub file_patterns: Option<Vec<Regex>>,
}

/// Collects template configurations from files named [`GENERATOR_FILE_NAME`] within the root level
/// directories in the provided path. This function gracefully handles any issues related to the
/// existence or format of the generator files, allowing the code to skip problematic starter templates
/// without returning an error. This approach is designed to avoid negatively impacting users due to
/// faulty template configurations.
///
/// # Errors
/// The code should returns an error only when could get folder collections.
pub fn collect_templates(path: &std::path::PathBuf) -> eyre::Result<BTreeMap<String, Template>> {
    tracing::debug!(
        path = path.display().to_string(),
        "collecting starters template"
    );

    let entries = fs::read_dir(path)?;

    let mut templates = BTreeMap::new();

    // Iterate over the entries and filter out directories
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(starter_folder) = entry.file_name().to_str() {
                let generator_file_path = std::path::Path::new(path)
                    .join(starter_folder)
                    .join(GENERATOR_FILE_NAME);

                let outer_span = tracing::info_span!(
                    "generator",
                    file = generator_file_path.display().to_string()
                );
                let _enter = outer_span.enter();

                tracing::debug!("parsing generator file");

                if !generator_file_path.exists() {
                    tracing::debug!("generator file not found");
                    continue;
                }

                tracing::debug!(path = generator_file_path.display().to_string(), "bla bla");
                let rdr = match std::fs::File::open(&generator_file_path) {
                    Ok(rdr) => rdr,
                    Err(e) => {
                        tracing::debug!(error = e.to_string(), "could not open generator file");
                        continue;
                    }
                };

                match serde_yaml::from_reader(&rdr) {
                    Ok(t) => templates.insert(starter_folder.to_string(), t),
                    Err(e) => {
                        tracing::debug!(error = e.to_string(), "invalid format");
                        continue;
                    }
                };
            }
        }
    }

    Ok(templates)
}

impl Template {
    /// Generates files based on the given template by recursively applying template rules to files
    /// within the specified path.
    ///
    /// # Description
    /// This method utilizes a parallel file walker to traverse the directory structure starting from
    /// the specified root path (`from`). For each file encountered, it checks whether the template
    /// rules should be applied based on file patterns. If the rules are applicable and an error occurs
    /// during the application, the error is logged, and the walker is instructed to quit processing
    /// further files in the current subtree.
    pub fn generate(&self, from: &PathBuf, args: &ArgsPlaceholder) {
        let walker = WalkBuilder::new(from).build_parallel();
        walker.run(|| {
            Box::new(move |result| {
                if let Ok(entry) = result {
                    let path = entry.path();

                    if !path.starts_with(from.join("target"))
                        && Self::should_run_file(path, self.file_patterns.as_ref())
                    {
                        if let Err(e) = self.apply_rules(path, args) {
                            tracing::info!(
                                error = e.to_string(),
                                path = path.display().to_string(),
                                "could not run rules placeholder replacement on the file"
                            );
                            return WalkState::Quit;
                        }
                    }
                }
                WalkState::Continue
            })
        });

        if let Err(err) = fs::remove_file(from.join(GENERATOR_FILE_NAME)) {
            tracing::debug!(error = err.to_string(), "could not delete generator file");
        }
    }

    /// Applies the specified rules to the content of a file, updating the file in-place with the modified content.
    ///
    /// # Description
    /// This method reads the content of the file specified by `file`, applies each rule from the template
    /// to the content, and saves the modified content back to the same file. The rules are only applied if
    /// the file passes the filtering conditions based on file patterns associated with each rule. If any rule
    /// results in modifications to the content, the file is updated; otherwise, it remains unchanged.
    ///
    fn apply_rules(&self, file: &std::path::Path, args: &ArgsPlaceholder) -> std::io::Result<()> {
        let mut content = String::new();
        fs::File::open(file)?.read_to_string(&mut content)?;

        let mut is_changed = false;
        for rule in &self.rules.clone().unwrap_or_default() {
            if Self::should_run_file(file, rule.file_patterns.as_ref())
                && rule.pattern.is_match(&content)
            {
                content = rule
                    .pattern
                    .replace_all(&content, rule.kind.get_val(args))
                    .to_string();
                is_changed = true;
            }
        }

        if is_changed {
            let mut modified_file = fs::File::create(file)?;
            modified_file.write_all(content.as_bytes())?;
        }

        Ok(())
    }

    /// Determines whether the template rules should be applied to the given file path based on a list of regex patterns.
    fn should_run_file(path: &std::path::Path, patterns: Option<&Vec<Regex>>) -> bool {
        let Some(patterns) = patterns else {
            return true;
        };
        if path.is_file() {
            for pattern in patterns {
                if pattern.is_match(&path.display().to_string()) {
                    return true;
                }
            }
        }
        false
    }
}
