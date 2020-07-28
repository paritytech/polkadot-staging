# frozen_string_literal: true

require 'changelogerator'
require 'git'
require 'erb'
require 'toml'

version = ENV['GITHUB_REF']
token = ENV['GITHUB_TOKEN']

polkadot_path = ENV['GITHUB_WORKSPACE'] + '/polkadot/'
pg = Git.open(polkadot_path)

# Generate an ERB renderer based on the template .erb file
renderer = ERB.new(
  File.read(ENV['GITHUB_WORKSPACE'] + '/polkadot/scripts/github/polkadot_release.erb'),
  trim_mode: '<>'
)

# get last polkadot version. Use handy Gem::Version for sorting by version
last_version = pg
              .tags
              .map(&:name)
              .grep(/^v\d+\.\d+\.\d+.*$/)
              .sort_by { |v| Gem::Version.new(v.slice(1...)) }[-2]

polkadot_cl = Changelog.new(
  's3krit/polkadot', version, last_version, token: token
)

# Get prev and cur substrate SHAs - parse the old and current Cargo.lock for
# polkadot and extract the sha that way.
prev_cargo = TOML::Parser.new(pg.show("#{last_version}:Cargo.lock")).parsed
current_cargo = TOML::Parser.new(pg.show("#{version}:Cargo.lock")).parsed

substrate_prev_sha = prev_cargo['package']
                    .find { |p| p['name'] == 'sc-cli' }['source']
                    .split('#').last

substrate_cur_sha = current_cargo['package']
                    .find { |p| p['name'] == 'sc-cli' }['source']
                    .split('#').last

substrate_cl = Changelog.new(
  'paritytech/substrate', substrate_prev_sha, substrate_cur_sha,
  token: token,
  prefix: true
)

all_changes = polkadot_cl.changes + substrate_cl.changes

# Set all the variables needed for a release

misc_changes = Changelog.changes_with_label(all_changes, 'B1-releasenotes')
client_changes = Changelog.changes_with_label(all_changes, 'B5-clientnoteworthy')
runtime_changes = Changelog.changes_with_label(all_changes, 'B7-runtimenoteworthy')

release_priority = Changelog.highest_priority_for_changes(all_changes)

# Pulled from the previous Github step
rustc_stable = 'v1.2.3'
rustc_nightly = 'v1.2.3'

polkadot_runtime = File.open(polkadot_path + '/runtime/polkadot/src/lib.rs')
                       .find { |l| l =~ /spec_version/ }
                       .match(/[0-9]+/)[0]
kusama_runtime = File.open(polkadot_path + '/runtime/kusama/src/lib.rs')
                       .find { |l| l =~ /spec_version/ }
                       .match(/[0-9]+/)[0]
westend_runtime = File.open(polkadot_path + '/runtime/westend/src/lib.rs')
                       .find { |l| l =~ /spec_version/ }
                       .match(/[0-9]+/)[0]
puts renderer.result
