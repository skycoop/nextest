// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{
    CompiledOverride, CompiledOverridesByProfile, CustomTestGroup, DeserializedOverride,
    RetryPolicy, SettingSource, SlowTimeout, TestGroup, TestGroupConfig, TestSettings, TestThreads,
    ThreadsRequired, ToolConfigFile,
};
use crate::{
    errors::{
        provided_by_tool, ConfigParseError, ConfigParseErrorKind, ProfileNotFound,
        UnknownTestGroupError,
    },
    platform::BuildPlatforms,
    reporter::{FinalStatusLevel, StatusLevel, TestOutputDisplay},
};
use camino::{Utf8Path, Utf8PathBuf};
use config::{builder::DefaultState, Config, ConfigBuilder, File, FileFormat, FileSourceFile};
use guppy::graph::PackageGraph;
use nextest_filtering::TestQuery;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

/// Gets the number of available CPUs and caches the value.
#[inline]
pub fn get_num_cpus() -> usize {
    static NUM_CPUS: Lazy<usize> = Lazy::new(|| match std::thread::available_parallelism() {
        Ok(count) => count.into(),
        Err(err) => {
            log::warn!("unable to determine num-cpus ({err}), assuming 1 logical CPU");
            1
        }
    });

    *NUM_CPUS
}

/// Overall configuration for nextest.
///
/// This is the root data structure for nextest configuration. Most runner-specific configuration is managed
/// through [profiles](NextestProfile), obtained through the [`profile`](Self::profile) method.
///
/// For more about configuration, see
/// [Configuration](https://nexte.st/book/configuration.html) in the nextest
/// book.
#[derive(Clone, Debug)]
pub struct NextestConfig {
    workspace_root: Utf8PathBuf,
    inner: NextestConfigImpl,
    overrides: CompiledOverridesByProfile,
}

impl NextestConfig {
    /// The default location of the config within the path: `.config/nextest.toml`, used to read the
    /// config from the given directory.
    pub const CONFIG_PATH: &'static str = ".config/nextest.toml";

    /// Contains the default config as a TOML file.
    ///
    /// Repository-specific configuration is layered on top of the default config.
    pub const DEFAULT_CONFIG: &'static str = include_str!("../../default-config.toml");

    /// Environment configuration uses this prefix, plus a _.
    pub const ENVIRONMENT_PREFIX: &'static str = "NEXTEST";

    /// The name of the default profile.
    pub const DEFAULT_PROFILE: &'static str = "default";

    /// The name of the default profile used for miri.
    pub const DEFAULT_MIRI_PROFILE: &'static str = "default-miri";

    /// A list containing the names of the Nextest defined reserved profile names.
    pub const DEFAULT_PROFILES: &'static [&'static str] =
        &[Self::DEFAULT_PROFILE, Self::DEFAULT_MIRI_PROFILE];

    /// Reads the nextest config from the given file, or if not specified from `.config/nextest.toml`
    /// in the workspace root.
    ///
    /// `tool_config_files` are lower priority than `config_file` but higher priority than the
    /// default config. Files in `tool_config_files` that come earlier are higher priority than those
    /// that come later.
    ///
    /// If no config files are specified and this file doesn't have `.config/nextest.toml`, uses the
    /// default config options.
    pub fn from_sources<'a, I>(
        workspace_root: impl Into<Utf8PathBuf>,
        graph: &PackageGraph,
        config_file: Option<&Utf8Path>,
        tool_config_files: impl IntoIterator<IntoIter = I>,
    ) -> Result<Self, ConfigParseError>
    where
        I: Iterator<Item = &'a ToolConfigFile> + DoubleEndedIterator,
    {
        Self::from_sources_impl(
            workspace_root,
            graph,
            config_file,
            tool_config_files.into_iter(),
            |config_file, tool, unknown| {
                let mut unknown_str = String::new();
                if unknown.len() == 1 {
                    // Print this on the same line.
                    unknown_str.push(' ');
                    unknown_str.push_str(unknown.iter().next().unwrap());
                } else {
                    for ignored_key in unknown {
                        unknown_str.push('\n');
                        unknown_str.push_str("  - ");
                        unknown_str.push_str(ignored_key);
                    }
                }

                log::warn!(
                    "ignoring unknown configuration keys in config file {config_file}{}:{unknown_str}",
                    provided_by_tool(tool),
                )
            },
        )
    }

    // A custom unknown_callback can be passed in while testing.
    fn from_sources_impl<'a, I>(
        workspace_root: impl Into<Utf8PathBuf>,
        graph: &PackageGraph,
        config_file: Option<&Utf8Path>,
        tool_config_files: impl IntoIterator<IntoIter = I>,
        mut unknown_callback: impl FnMut(&Utf8Path, Option<&str>, &BTreeSet<String>),
    ) -> Result<Self, ConfigParseError>
    where
        I: Iterator<Item = &'a ToolConfigFile> + DoubleEndedIterator,
    {
        let workspace_root = workspace_root.into();
        let tool_config_files_rev = tool_config_files.into_iter().rev();
        let (inner, overrides) = Self::read_from_sources(
            graph,
            &workspace_root,
            config_file,
            tool_config_files_rev,
            &mut unknown_callback,
        )?;
        Ok(Self {
            workspace_root,
            inner,
            overrides,
        })
    }

    /// Returns the default nextest config.
    #[cfg(test)]
    pub(crate) fn default_config(workspace_root: impl Into<Utf8PathBuf>) -> Self {
        use itertools::Itertools;

        let config = Self::make_default_config()
            .build()
            .expect("default config is always valid");

        let mut unknown = BTreeSet::new();
        let deserialized: NextestConfigDeserialize =
            serde_ignored::deserialize(config, |path: serde_ignored::Path| {
                unknown.insert(path.to_string());
            })
            .expect("default config is always valid");

        // Make sure there aren't any unknown keys in the default config, since it is
        // embedded/shipped with this binary.
        if !unknown.is_empty() {
            panic!(
                "found unknown keys in default config: {}",
                unknown.iter().join(", ")
            );
        }

        Self {
            workspace_root: workspace_root.into(),
            inner: deserialized.into_config_impl(),
            // The default config does not (cannot) have overrides.
            overrides: CompiledOverridesByProfile::default(),
        }
    }

    /// Returns the profile with the given name, or an error if a profile was specified but not
    /// found.
    pub fn profile(
        &self,
        name: impl AsRef<str>,
    ) -> Result<NextestProfile<'_, PreBuildPlatform>, ProfileNotFound> {
        self.make_profile(name.as_ref())
    }

    // ---
    // Helper methods
    // ---

    fn read_from_sources<'a>(
        graph: &PackageGraph,
        workspace_root: &Utf8Path,
        file: Option<&Utf8Path>,
        tool_config_files_rev: impl Iterator<Item = &'a ToolConfigFile>,
        unknown_callback: &mut impl FnMut(&Utf8Path, Option<&str>, &BTreeSet<String>),
    ) -> Result<(NextestConfigImpl, CompiledOverridesByProfile), ConfigParseError> {
        // First, get the default config.
        let mut composite_builder = Self::make_default_config();

        // Overrides are handled additively.
        // Note that they're stored in reverse order here, and are flipped over at the end.
        let mut overrides = CompiledOverridesByProfile::default();

        let mut known_groups = BTreeSet::new();

        // Next, merge in tool configs.
        for ToolConfigFile { config_file, tool } in tool_config_files_rev {
            let source = File::new(config_file.as_str(), FileFormat::Toml);
            Self::deserialize_individual_config(
                graph,
                workspace_root,
                config_file,
                Some(tool),
                source.clone(),
                &mut overrides,
                unknown_callback,
                &mut known_groups,
            )?;

            // This is the final, composite builder used at the end.
            composite_builder = composite_builder.add_source(source);
        }

        // Next, merge in the config from the given file.
        let (config_file, source) = match file {
            Some(file) => (file.to_owned(), File::new(file.as_str(), FileFormat::Toml)),
            None => {
                let config_file = workspace_root.join(Self::CONFIG_PATH);
                let source = File::new(config_file.as_str(), FileFormat::Toml).required(false);
                (config_file, source)
            }
        };

        Self::deserialize_individual_config(
            graph,
            workspace_root,
            &config_file,
            None,
            source.clone(),
            &mut overrides,
            unknown_callback,
            &mut known_groups,
        )?;

        composite_builder = composite_builder.add_source(source);

        // The unknown set is ignored here because any values in it have already been reported in
        // deserialize_individual_config.
        let (config, _unknown) = Self::build_and_deserialize_config(&composite_builder)
            .map_err(|kind| ConfigParseError::new(config_file, None, kind))?;

        // Reverse all the overrides at the end.
        overrides.default.reverse();
        for override_ in overrides.other.values_mut() {
            override_.reverse();
        }

        Ok((config.into_config_impl(), overrides))
    }

    #[allow(clippy::too_many_arguments)]
    fn deserialize_individual_config(
        graph: &PackageGraph,
        workspace_root: &Utf8Path,
        config_file: &Utf8Path,
        tool: Option<&str>,
        source: File<FileSourceFile, FileFormat>,
        overrides_out: &mut CompiledOverridesByProfile,
        unknown_callback: &mut impl FnMut(&Utf8Path, Option<&str>, &BTreeSet<String>),
        known_groups: &mut BTreeSet<CustomTestGroup>,
    ) -> Result<(), ConfigParseError> {
        // Try building default builder + this file to get good error attribution and handle
        // overrides additively.
        let default_builder = Self::make_default_config();
        let this_builder = default_builder.add_source(source);
        let (this_config, unknown) = Self::build_and_deserialize_config(&this_builder)
            .map_err(|kind| ConfigParseError::new(config_file, tool, kind))?;

        if !unknown.is_empty() {
            unknown_callback(config_file, tool, &unknown);
        }

        // Check that test groups are named as expected.
        let (valid_groups, invalid_groups): (BTreeSet<_>, _) =
            this_config.test_groups.keys().cloned().partition(|group| {
                if let Some(tool) = tool {
                    // The first component must be the tool name.
                    group
                        .as_identifier()
                        .tool_components()
                        .map_or(false, |(tool_name, _)| tool_name == tool)
                } else {
                    // If a tool is not specified, it must *not* be a tool identifier.
                    !group.as_identifier().is_tool_identifier()
                }
            });

        if !invalid_groups.is_empty() {
            let kind = if tool.is_some() {
                ConfigParseErrorKind::InvalidTestGroupsDefinedByTool(invalid_groups)
            } else {
                ConfigParseErrorKind::InvalidTestGroupsDefined(invalid_groups)
            };
            return Err(ConfigParseError::new(config_file, tool, kind));
        }

        known_groups.extend(valid_groups);

        let this_config = this_config.into_config_impl();

        let unknown_default_profiles: Vec<_> = this_config
            .all_profiles()
            .filter(|p| p.starts_with("default-") && !NextestConfig::DEFAULT_PROFILES.contains(p))
            .collect();
        if !unknown_default_profiles.is_empty() {
            log::warn!(
                "unknown profiles in the reserved `default-` namespace in config file {}{}:",
                config_file
                    .strip_prefix(workspace_root)
                    .unwrap_or(config_file),
                provided_by_tool(tool),
            );

            for profile in unknown_default_profiles {
                log::warn!("  {profile}");
            }
        }

        // Compile the overrides for this file.
        let this_overrides = CompiledOverridesByProfile::new(graph, &this_config)
            .map_err(|kind| ConfigParseError::new(config_file, tool, kind))?;

        // Check that all overrides specify known test groups.
        let mut unknown_group_errors = Vec::new();
        let mut check_test_group = |profile_name: &str, test_group: Option<&TestGroup>| {
            if let Some(TestGroup::Custom(group)) = test_group {
                if !known_groups.contains(group) {
                    unknown_group_errors.push(UnknownTestGroupError {
                        profile_name: profile_name.to_owned(),
                        name: TestGroup::Custom(group.clone()),
                    });
                }
            }
        };

        this_overrides.default.iter().for_each(|override_| {
            check_test_group("default", override_.data.test_group.as_ref());
        });

        // Check that override test groups are known.
        this_overrides
            .other
            .iter()
            .for_each(|(profile_name, overrides)| {
                overrides.iter().for_each(|override_| {
                    check_test_group(profile_name, override_.data.test_group.as_ref());
                });
            });

        // If there were any unknown groups, error out.
        if !unknown_group_errors.is_empty() {
            let known_groups = TestGroup::make_all_groups(known_groups.iter().cloned()).collect();
            return Err(ConfigParseError::new(
                config_file,
                tool,
                ConfigParseErrorKind::UnknownTestGroups {
                    errors: unknown_group_errors,
                    known_groups,
                },
            ));
        }

        // Grab the overrides for this config. Add them in reversed order (we'll flip it around at the end).
        overrides_out
            .default
            .extend(this_overrides.default.into_iter().rev());
        for (name, overrides) in this_overrides.other {
            overrides_out
                .other
                .entry(name)
                .or_default()
                .extend(overrides.into_iter().rev());
        }

        Ok(())
    }

    fn make_default_config() -> ConfigBuilder<DefaultState> {
        Config::builder().add_source(File::from_str(Self::DEFAULT_CONFIG, FileFormat::Toml))
    }

    fn make_profile(
        &self,
        name: &str,
    ) -> Result<NextestProfile<'_, PreBuildPlatform>, ProfileNotFound> {
        let custom_profile = self.inner.get_profile(name)?;

        // The profile was found: construct the NextestProfile.
        let mut store_dir = self.workspace_root.join(&self.inner.store.dir);
        store_dir.push(name);

        // Grab the overrides as well.
        let overrides = self
            .overrides
            .other
            .get(name)
            .into_iter()
            .flatten()
            .chain(self.overrides.default.iter())
            .cloned()
            .collect();

        Ok(NextestProfile {
            store_dir,
            default_profile: &self.inner.default_profile,
            custom_profile,
            test_groups: &self.inner.test_groups,
            overrides,
        })
    }

    /// This returns a tuple of (config, ignored paths).
    fn build_and_deserialize_config(
        builder: &ConfigBuilder<DefaultState>,
    ) -> Result<(NextestConfigDeserialize, BTreeSet<String>), ConfigParseErrorKind> {
        let config = builder
            .build_cloned()
            .map_err(|error| ConfigParseErrorKind::BuildError(Box::new(error)))?;

        let mut ignored = BTreeSet::new();
        let mut cb = |path: serde_ignored::Path| {
            ignored.insert(path.to_string());
        };
        let ignored_de = serde_ignored::Deserializer::new(config, &mut cb);
        let config: NextestConfigDeserialize = serde_path_to_error::deserialize(ignored_de)
            .map_err(|error| ConfigParseErrorKind::DeserializeError(Box::new(error)))?;

        Ok((config, ignored))
    }
}

/// The state of nextest profiles before build platforms have been applied.
#[derive(Clone, Debug)]
pub struct PreBuildPlatform {}

/// The state of nextest profiles after build platforms have been applied.
#[derive(Clone, Debug)]
pub struct FinalConfig {
    pub(super) host_eval: bool,
    pub(super) target_eval: bool,
}

/// A configuration profile for nextest. Contains most configuration used by the nextest runner.
///
/// Returned by [`NextestConfig::profile`].
#[derive(Clone, Debug)]
pub struct NextestProfile<'cfg, State = FinalConfig> {
    store_dir: Utf8PathBuf,
    default_profile: &'cfg DefaultProfileImpl,
    custom_profile: Option<&'cfg CustomProfileImpl>,
    test_groups: &'cfg BTreeMap<CustomTestGroup, TestGroupConfig>,
    pub(super) overrides: Vec<CompiledOverride<State>>,
}

impl<'cfg, State> NextestProfile<'cfg, State> {
    /// Returns the absolute profile-specific store directory.
    pub fn store_dir(&self) -> &Utf8Path {
        &self.store_dir
    }

    /// Returns the test group configuration for this profile.
    pub fn test_group_config(&self) -> &'cfg BTreeMap<CustomTestGroup, TestGroupConfig> {
        self.test_groups
    }

    #[allow(dead_code)]
    pub(super) fn custom_profile(&self) -> Option<&'cfg CustomProfileImpl> {
        self.custom_profile
    }
}

impl<'cfg> NextestProfile<'cfg, PreBuildPlatform> {
    /// Applies build platforms to make the profile ready for evaluation.
    ///
    /// This is a separate step from parsing the config and reading a profile so that cargo-nextest
    /// can tell users about configuration parsing errors before building the binary list.
    pub fn apply_build_platforms(self, build_platforms: &BuildPlatforms) -> NextestProfile<'cfg> {
        let overrides = self
            .overrides
            .into_iter()
            .map(|override_| override_.apply_build_platforms(build_platforms))
            .collect();
        NextestProfile {
            store_dir: self.store_dir,
            default_profile: self.default_profile,
            custom_profile: self.custom_profile,
            test_groups: self.test_groups,
            overrides,
        }
    }
}

impl<'cfg> NextestProfile<'cfg, FinalConfig> {
    /// Returns the retry count for this profile.
    pub fn retries(&self) -> RetryPolicy {
        self.custom_profile
            .and_then(|profile| profile.retries)
            .unwrap_or(self.default_profile.retries)
    }

    /// Returns the number of threads to run against for this profile.
    pub fn test_threads(&self) -> TestThreads {
        self.custom_profile
            .and_then(|profile| profile.test_threads)
            .unwrap_or(self.default_profile.test_threads)
    }

    /// Returns the number of threads required for each test.
    pub fn threads_required(&self) -> ThreadsRequired {
        self.custom_profile
            .and_then(|profile| profile.threads_required)
            .unwrap_or(self.default_profile.threads_required)
    }

    /// Returns the time after which tests are treated as slow for this profile.
    pub fn slow_timeout(&self) -> SlowTimeout {
        self.custom_profile
            .and_then(|profile| profile.slow_timeout)
            .unwrap_or(self.default_profile.slow_timeout)
    }

    /// Returns the time after which a child process that hasn't closed its handles is marked as
    /// leaky.
    pub fn leak_timeout(&self) -> Duration {
        self.custom_profile
            .and_then(|profile| profile.leak_timeout)
            .unwrap_or(self.default_profile.leak_timeout)
    }

    /// Returns the test status level.
    pub fn status_level(&self) -> StatusLevel {
        self.custom_profile
            .and_then(|profile| profile.status_level)
            .unwrap_or(self.default_profile.status_level)
    }

    /// Returns the test status level at the end of the run.
    pub fn final_status_level(&self) -> FinalStatusLevel {
        self.custom_profile
            .and_then(|profile| profile.final_status_level)
            .unwrap_or(self.default_profile.final_status_level)
    }

    /// Returns the failure output config for this profile.
    pub fn failure_output(&self) -> TestOutputDisplay {
        self.custom_profile
            .and_then(|profile| profile.failure_output)
            .unwrap_or(self.default_profile.failure_output)
    }

    /// Returns the failure output config for this profile.
    pub fn success_output(&self) -> TestOutputDisplay {
        self.custom_profile
            .and_then(|profile| profile.success_output)
            .unwrap_or(self.default_profile.success_output)
    }

    /// Returns the fail-fast config for this profile.
    pub fn fail_fast(&self) -> bool {
        self.custom_profile
            .and_then(|profile| profile.fail_fast)
            .unwrap_or(self.default_profile.fail_fast)
    }

    /// Returns settings for individual tests.
    pub fn settings_for(&self, query: &TestQuery<'_>) -> TestSettings {
        TestSettings::new(self, query)
    }

    /// Returns override settings for individual tests, with sources attached.
    pub(crate) fn settings_with_source_for(
        &self,
        query: &TestQuery<'_>,
    ) -> TestSettings<SettingSource<'_>> {
        TestSettings::new(self, query)
    }

    /// Returns the JUnit configuration for this profile.
    pub fn junit(&self) -> Option<NextestJunitConfig<'cfg>> {
        let path = self
            .custom_profile
            .map(|profile| &profile.junit.path)
            .unwrap_or(&self.default_profile.junit.path)
            .as_deref();

        path.map(|path| {
            let path = self.store_dir.join(path);
            let report_name = self
                .custom_profile
                .and_then(|profile| profile.junit.report_name.as_deref())
                .unwrap_or(&self.default_profile.junit.report_name);
            let store_success_output = self
                .custom_profile
                .and_then(|profile| profile.junit.store_success_output)
                .unwrap_or(self.default_profile.junit.store_success_output);
            let store_failure_output = self
                .custom_profile
                .and_then(|profile| profile.junit.store_failure_output)
                .unwrap_or(self.default_profile.junit.store_failure_output);
            NextestJunitConfig {
                path,
                report_name,
                store_success_output,
                store_failure_output,
            }
        })
    }
}

/// JUnit configuration for nextest, returned by a [`NextestProfile`].
#[derive(Clone, Debug)]
pub struct NextestJunitConfig<'cfg> {
    path: Utf8PathBuf,
    report_name: &'cfg str,
    store_success_output: bool,
    store_failure_output: bool,
}

impl<'cfg> NextestJunitConfig<'cfg> {
    /// Returns the absolute path to the JUnit report.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Returns the name of the JUnit report.
    pub fn report_name(&self) -> &'cfg str {
        self.report_name
    }

    /// Returns true if success output should be stored.
    pub fn store_success_output(&self) -> bool {
        self.store_success_output
    }

    /// Returns true if failure output should be stored.
    pub fn store_failure_output(&self) -> bool {
        self.store_failure_output
    }
}

#[derive(Clone, Debug)]
pub(super) struct NextestConfigImpl {
    store: StoreConfigImpl,
    test_groups: BTreeMap<CustomTestGroup, TestGroupConfig>,
    default_profile: DefaultProfileImpl,
    other_profiles: HashMap<String, CustomProfileImpl>,
}

impl NextestConfigImpl {
    fn get_profile(&self, profile: &str) -> Result<Option<&CustomProfileImpl>, ProfileNotFound> {
        let custom_profile = match profile {
            NextestConfig::DEFAULT_PROFILE => None,
            other => Some(
                self.other_profiles
                    .get(other)
                    .ok_or_else(|| ProfileNotFound::new(profile, self.all_profiles()))?,
            ),
        };
        Ok(custom_profile)
    }

    fn all_profiles(&self) -> impl Iterator<Item = &str> {
        self.other_profiles
            .keys()
            .map(|key| key.as_str())
            .chain(std::iter::once(NextestConfig::DEFAULT_PROFILE))
    }

    pub(super) fn default_profile(&self) -> &DefaultProfileImpl {
        &self.default_profile
    }

    pub(super) fn other_profiles(&self) -> impl Iterator<Item = (&str, &CustomProfileImpl)> {
        self.other_profiles
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }
}

// This is the form of `NextestConfig` that gets deserialized.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct NextestConfigDeserialize {
    store: StoreConfigImpl,
    #[serde(default)]
    test_groups: BTreeMap<CustomTestGroup, TestGroupConfig>,
    #[serde(rename = "profile")]
    profiles: HashMap<String, CustomProfileImpl>,
}

impl NextestConfigDeserialize {
    fn into_config_impl(mut self) -> NextestConfigImpl {
        let p = self
            .profiles
            .remove("default")
            .expect("default profile should exist");
        let default_profile = DefaultProfileImpl::new(p);

        NextestConfigImpl {
            store: self.store,
            default_profile,
            test_groups: self.test_groups,
            other_profiles: self.profiles,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct StoreConfigImpl {
    dir: Utf8PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DefaultProfileImpl {
    test_threads: TestThreads,
    threads_required: ThreadsRequired,
    retries: RetryPolicy,
    status_level: StatusLevel,
    final_status_level: FinalStatusLevel,
    failure_output: TestOutputDisplay,
    success_output: TestOutputDisplay,
    fail_fast: bool,
    slow_timeout: SlowTimeout,
    leak_timeout: Duration,
    overrides: Vec<DeserializedOverride>,
    junit: DefaultJunitImpl,
}

impl DefaultProfileImpl {
    fn new(p: CustomProfileImpl) -> Self {
        Self {
            test_threads: p
                .test_threads
                .expect("test-threads present in default profile"),
            threads_required: p
                .threads_required
                .expect("threads-required present in default profile"),
            retries: p.retries.expect("retries present in default profile"),
            status_level: p
                .status_level
                .expect("status-level present in default profile"),
            final_status_level: p
                .final_status_level
                .expect("final-status-level present in default profile"),
            failure_output: p
                .failure_output
                .expect("failure-output present in default profile"),
            success_output: p
                .success_output
                .expect("success-output present in default profile"),
            fail_fast: p.fail_fast.expect("fail-fast present in default profile"),
            slow_timeout: p
                .slow_timeout
                .expect("slow-timeout present in default profile"),
            leak_timeout: p
                .leak_timeout
                .expect("leak-timeout present in default profile"),
            overrides: p.overrides,
            junit: DefaultJunitImpl {
                path: p.junit.path,
                report_name: p
                    .junit
                    .report_name
                    .expect("junit.report present in default profile"),
                store_success_output: p
                    .junit
                    .store_success_output
                    .expect("junit.store-success-output present in default profile"),
                store_failure_output: p
                    .junit
                    .store_failure_output
                    .expect("junit.store-failure-output present in default profile"),
            },
        }
    }

    pub(super) fn overrides(&self) -> &[DeserializedOverride] {
        &self.overrides
    }
}

#[derive(Clone, Debug)]
struct DefaultJunitImpl {
    path: Option<Utf8PathBuf>,
    report_name: String,
    store_success_output: bool,
    store_failure_output: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct CustomProfileImpl {
    #[serde(default, deserialize_with = "super::deserialize_retry_policy")]
    retries: Option<RetryPolicy>,
    #[serde(default)]
    test_threads: Option<TestThreads>,
    #[serde(default)]
    threads_required: Option<ThreadsRequired>,
    #[serde(default)]
    status_level: Option<StatusLevel>,
    #[serde(default)]
    final_status_level: Option<FinalStatusLevel>,
    #[serde(default)]
    failure_output: Option<TestOutputDisplay>,
    #[serde(default)]
    success_output: Option<TestOutputDisplay>,
    #[serde(default)]
    fail_fast: Option<bool>,
    #[serde(default, deserialize_with = "super::deserialize_slow_timeout")]
    slow_timeout: Option<SlowTimeout>,
    #[serde(default, with = "humantime_serde::option")]
    leak_timeout: Option<Duration>,
    #[serde(default)]
    overrides: Vec<DeserializedOverride>,
    #[serde(default)]
    junit: JunitImpl,
}

#[allow(dead_code)]
impl CustomProfileImpl {
    pub(super) fn test_threads(&self) -> Option<TestThreads> {
        self.test_threads
    }

    pub(super) fn overrides(&self) -> &[DeserializedOverride] {
        &self.overrides
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JunitImpl {
    #[serde(default)]
    path: Option<Utf8PathBuf>,
    #[serde(default)]
    report_name: Option<String>,
    #[serde(default)]
    store_success_output: Option<bool>,
    #[serde(default)]
    store_failure_output: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_helpers::*;
    use camino_tempfile::tempdir;

    #[test]
    fn default_config_is_valid() {
        let default_config = NextestConfig::default_config("foo");
        default_config
            .profile(NextestConfig::DEFAULT_PROFILE)
            .expect("default profile should exist");
    }

    #[test]
    fn ignored_keys() {
        let config_contents = r#"
        ignored1 = "test"

        [profile.default]
        retries = 3
        ignored2 = "hi"

        [[profile.default.overrides]]
        filter = 'test(test_foo)'
        retries = 20
        ignored3 = 42
        "#;

        let tool_config_contents = r#"
        [store]
        ignored4 = 20

        [profile.default]
        retries = 4
        ignored5 = false

        [profile.tool]
        retries = 12

        [[profile.tool.overrides]]
        filter = 'test(test_baz)'
        retries = 22
        ignored6 = 6.5
        "#;

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let workspace_root = graph.workspace().root();
        let tool_path = workspace_root.join(".config/tool.toml");
        std::fs::write(&tool_path, tool_config_contents).unwrap();

        let mut unknown_keys = HashMap::new();

        let _ = NextestConfig::from_sources_impl(
            workspace_root,
            &graph,
            None,
            &[ToolConfigFile {
                tool: "my-tool".to_owned(),
                config_file: tool_path,
            }][..],
            |_path, tool, ignored| {
                unknown_keys.insert(tool.map(|s| s.to_owned()), ignored.clone());
            },
        )
        .expect("config is valid");

        assert_eq!(
            unknown_keys.len(),
            2,
            "there are two files with unknown keys"
        );

        let keys = unknown_keys
            .remove(&None)
            .expect("unknown keys for .config/nextest.toml");
        assert_eq!(
            keys,
            maplit::btreeset! {
                "ignored1".to_owned(),
                "profile.default.ignored2".to_owned(),
                "profile.default.overrides.0.ignored3".to_owned(),
            }
        );

        let keys = unknown_keys
            .remove(&Some("my-tool".to_owned()))
            .expect("unknown keys for my-tool");
        assert_eq!(
            keys,
            maplit::btreeset! {
                "store.ignored4".to_owned(),
                "profile.default.ignored5".to_owned(),
                "profile.tool.overrides.0.ignored6".to_owned(),
            }
        );
    }
}
