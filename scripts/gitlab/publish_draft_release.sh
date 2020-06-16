#!/usr/bin/env bash

# shellcheck source=lib.sh
source "$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )/lib.sh"

# Set initial variables
substrate_repo="https://github.com/paritytech/substrate"
substrate_dir='./substrate'

# Cloning repos to ensure freshness
echo "[+] Cloning substrate to generate list of changes"
git clone $substrate_repo $substrate_dir
echo "[+] Finished cloning substrate into $substrate_dir"

version="$CI_COMMIT_TAG"
last_version=$(git tag -l | sort -V | grep -B 1 -x "$CI_COMMIT_TAG" | head -n 1)
echo "[+] Version: $version; Previous version: $last_version"

# Check that a signed tag exists on github for this version
echo '[+] Checking tag has been signed'
check_tag "paritytech/polkadot" "$version"
case $? in
  0) echo '[+] Tag found and has been signed'
    ;;
  1) echo '[!] Tag found but has not been signed. Aborting release.'; exit 1
    ;;
  2) echo '[!] Tag not found. Aborting release.'; exit 1
esac

# Pull rustc version used by rust-builder for stable and nightly
stable_rustc="$(rustc +stable --version)"
nightly_rustc="$(rustc +nightly --version)"

# Start with referencing current native runtime
# and find any referenced PRs since last release
# Note: Drop any changes that begin with '[contracts]' or 'contracts:'
polkadot_spec=$(grep spec_version runtime/polkadot/src/lib.rs | tail -n 1 | grep -Eo '[0-9]+')
echo "[+] Polkadot spec version: $polkadot_spec"
kusama_spec=$(grep spec_version runtime/kusama/src/lib.rs | tail -n 1 | grep -Eo '[0-9]+')
echo "[+] Kusama spec version: $kusama_spec"
westend_spec=$(grep spec_version runtime/westend/src/lib.rs | tail -n 1 | grep -Eo '[0-9]+')
echo "[+] Westend spec version: $westend_spec"
release_text="Polkadot native runtime: $polkadot_spec

Kusama native runtime: $kusama_spec

Westend native runtime: $westend_spec

This release was built with the following versions of \`rustc\`. Other versions may work.
- $stable_rustc
- $nightly_rustc
"

runtime_changes=""

# Following variables are for tracking the priority of the release (i.e.,
# how important it is for the user to upgrade).
# It's frustrating that we need to make an array of indexes (in this case the
# labels), but it's necessary to maintain the correct order. Labels and
# descriptions *must* be kept in lockstep

priority_labels=(
  'C1-low'
  'C3-medium'
  'C7-high'
  'C9-critical'
)

declare -A priority_descriptions=(
['C1-low']="Upgrade priority: Low (upgrade at your convenience)"
['C3-medium']="Upgrade priority: *Medium* (timely upgrade recommended)"
['C7-high']="Upgrade priority:❗ **HIGH** ❗ Please upgrade your node as soon as possible"
['C9-critical']="Upgrade priority: ❗❗ **URGENT** ❗❗ PLEASE UPGRADE IMMEDIATELY"
)

max_label=-1
priority="${priority_descriptions['C1-low']}"
declare -a priority_changes

# Iterate through every PR
while IFS= read -r line; do
  pr_id=$(echo "$line" | sed -E 's/.*#([0-9]+)\)$/\1/')

  # Release priority check:
  # For each PR, we look for every label equal to or higher than the current highest
  # I.e., if there has already been a PR marked as 'medium', we only need
  # to look for priorities medium or above. If we find one, we set the
  # priority to that level.
  for ((index=max_label; index<${#priority_labels[@]}; index++)) ; do
    cur_label="${priority_labels[$index]}"
    echo "[+] Checking #$pr_id for presence of $cur_label label"
    if has_label 'paritytech/polkadot' "$pr_id" "$cur_label" ; then
      echo "[+] #$pr_id has label $cur_label. Setting max."
      prev_label="$max_label"
      max_label="$index"
      priority="${priority_descriptions[$cur_label]}"
      # If it's not an increase in priority, we just append the PR to the list
      if [ "$prev_label" == "$max_label" ]; then
        priority_changes+=("#$pr_id")
      fi
      # If the priority has increased, we override previous changes with new changes
      if [ "$prev_label" != "$max_label" ]; then
        priority_changes=("#$pr_id")
      fi
    fi
  done

  # If the PR is labelled silent, we can do an early continue to save a little work
  if has_label 'paritytech/polkadot' "$pr_id" 'B0-silent'; then
    continue
  fi

  # If the PR has a runtimenoteworthy label, add to the runtime_changes section
  if has_label 'paritytech/polkadot' "$pr_id" 'B2-runtimenoteworthy'; then
    runtime_changes="$runtime_changes
$line"
  else
  # otherwise, add the PR to the main list of changes
  release_text="$release_text
$line"
  fi
done <<< "$(sanitised_git_logs "$last_version" "$version" | \
  sed '/^\[contracts\].*/d' | \
  sed '/^contracts:.*/d' )"

if [ -n "$runtime_changes" ]; then
    release_text="$release_text

## Runtime
$runtime_changes"
fi
echo "$release_text"

# Get substrate changes between last polkadot version and current
# By grepping the Cargo.lock for a substrate crate, and grepping out the commit hash
cur_substrate_commit=$(grep -A 2 'name = "sc-cli"' Cargo.lock | grep -E -o '[a-f0-9]{40}')
old_substrate_commit=$(git diff "refs/tags/$last_version" Cargo.lock |\
  grep -A 2 'name = "sc-cli"' | grep -E -o '[a-f0-9]{40}')
pushd $substrate_dir || exit
  git checkout master > /dev/null
  git pull > /dev/null
  all_substrate_changes="$(sanitised_git_logs "$old_substrate_commit" "$cur_substrate_commit" | sed 's/(#/(paritytech\/substrate#/')"
  substrate_runtime_changes=""
  substrate_api_changes=""
  substrate_client_changes=""
  substrate_changes=""

  # Set initial upgrade priority variables
  substrate_max_label=-1
  substrate_priority="${priority_descriptions['C1-low']}"

  declare -a substrate_priority_changes

  echo "[+] Iterating through substrate changes to find labelled PRs"
  while IFS= read -r line; do
    pr_id=$(echo "$line" | sed -E 's/.*#([0-9]+)\)$/\1/')

    # Basically same check as Polkadot priority
    # We only need to check for any labels of the current priority or higher
    for ((index=substrate_max_label; index<${#priority_labels[@]}; index++)) ; do
      cur_label="${priority_labels[$index]}"
      echo "[+] Checking substrate/#$pr_id for presence of $cur_label label"
      if has_label 'paritytech/substrate' "$pr_id" "$cur_label" ; then
        echo "[+] #$pr_id has label $cur_label. Setting max."
        prev_label="$substrate_max_label"
        substrate_max_label="$index"
        substrate_priority="${priority_descriptions[$cur_label]}"
        # If it's not an increase in priority, we just append
        if [ "$prev_label" == "$max_label" ]; then
          substrate_priority_changes+=("paritytech/substrate#$pr_id")
        fi
        # If the priority has increased, we override previous changes with new changes
        if [ "$prev_label" != "$max_label" ]; then
          substrate_priority_changes=("paritytech/substrate#$pr_id")
        fi
      fi
    done

    # Skip if the PR has the silent label - this allows us to skip a few requests
    if has_label 'paritytech/substrate' "$pr_id" 'B0-silent'; then
      continue
    fi
    if has_label 'paritytech/substrate' "$pr_id" 'B7-runtimenoteworthy'; then
      substrate_runtime_changes="$substrate_runtime_changes
$line"
    fi
    if has_label 'paritytech/substrate' "$pr_id" 'B5-clientnoteworthy'; then
      substrate_client_changes="$substrate_client_changes
$line"
    fi
     if has_label 'paritytech/substrate' "$pr_id" 'B3-apinoteworthy' ; then
      substrate_api_changes="$substrate_api_changes
$line"
      continue
    fi
  done <<< "$all_substrate_changes"
popd || exit

# Make the substrate section if there are any substrate changes
if [ -n "$substrate_runtime_changes" ] ||
   [ -n "$substrate_api_changes" ] ||
   [ -n "$substrate_client_changes" ]; then
  substrate_changes=$(cat << EOF
# Substrate changes

EOF
)
  if [ -n "$substrate_runtime_changes" ]; then
    substrate_changes="$substrate_changes

## Runtime
$substrate_runtime_changes"
  fi
  if [ -n "$substrate_client_changes" ]; then
    substrate_changes="$substrate_changes

## Client
$substrate_client_changes"
  fi
  if [ -n "$substrate_api_changes" ]; then
    substrate_changes="$substrate_changes

## API
$substrate_api_changes"
  fi
  release_text="$release_text

$substrate_changes"
fi

# Finally, add the priorities to the *start* of the release notes
# If polkadot and substrate priority = low, no need for list of changes
if [ "$priority" == "${priority_descriptions['C1-low']}" ] &&
   [ "$substrate_priority" == "${priority_descriptions['C1-low']}" ]; then
  release_text="$priority

$release_text"
else
  release_text="$priority - due to change(s): ${priority_changes[*]} ${substrate_priority_changes[*]}

$release_text"
fi

echo "[+] Release text generated: "
echo "$release_text"

echo "[+] Pushing release to github"
# Create release on github
release_name="Polkadot CC1 $version"
data=$(jq -Rs --arg version "$version" \
  --arg release_name "$release_name" \
  --arg release_text "$release_text" \
'{
  "tag_name": $version,
  "target_commitish": "master",
  "name": $release_name,
  "body": $release_text,
  "draft": true,
  "prerelease": false
}' < /dev/null)

out=$(curl -s -X POST --data "$data" -H "Authorization: token $GITHUB_RELEASE_TOKEN" "$api_base/paritytech/polkadot/releases")

html_url=$(echo "$out" | jq -r .html_url)

if [ "$html_url" == "null" ]
then
  echo "[!] Something went wrong posting:"
  echo "$out"
  # If we couldn't post, don't want to announce in Matrix
  exit 1
else
  echo "[+] Release draft created: $html_url"
fi

echo '[+] Sending draft release URL to Matrix'

msg_body=$(cat <<EOF
**New version of polkadot tagged:** $CI_COMMIT_TAG.
Gav: Draft release created: $html_url
Build pipeline: $CI_PIPELINE_URL
EOF
)
formatted_msg_body=$(cat <<EOF
<strong>New version of polkadot tagged:</strong> $CI_COMMIT_TAG<br />
Gav: Draft release created: $html_url <br />
Build pipeline: $CI_PIPELINE_URL
EOF
)
send_message "$(structure_message "$msg_body" "$formatted_msg_body")" "$MATRIX_ROOM_ID" "$MATRIX_ACCESS_TOKEN"

echo "[+] Done! Maybe the release worked..."
