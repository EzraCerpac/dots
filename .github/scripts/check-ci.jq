def group($id; $selected; $names; $placeholder):
  {id: $id, selected: $selected, names: $names, placeholder: $placeholder};

(env.RESULTS | fromjson) as $needs
| ($needs.changes.outputs.matrix | fromjson) as $matrix
| ($needs.changes.outputs.code | fromjson) as $code
| ($needs.changes.outputs.full | fromjson) as $full
| ($code and $full) as $extended
| [
    group("changes"; true; ["Classify changes"]; null),
    group("secrets"; true; ["Secret scan"]; null),
    group("lint"; $code; ["Rust lint"]; null),
    group("native"; $code; [$matrix.linux.include[] | "Rust tests (\(.target))"]; "Rust tests (${{ matrix.target }})"),
    group("distros"; $code; ["Fedora", "Arch", "Alpine"] | map("Rust tests (\(.))"); "Rust tests (${{ matrix.distro }})"),
    group("mac-pr"; $code and ($full | not); ["Rust tests (aarch64-apple-darwin)"]; null),
    group("mac-build"; $extended; [$matrix.mac.include[] | "Build Cargo tests (\(.target))"]; "Build Cargo tests (${{ matrix.target }})"),
    group("mac-tests"; $extended; [$matrix.shards.include[] | "Rust tests (\(.target) shard \(.shard)/2)"]; "Rust tests (${{ matrix.target }} shard ${{ matrix.shard }}/2)"),
    group("oracle"; $extended; [$matrix.oracle.include[] | "v0.4.0 oracle (\(.platform))"]; "v0.4.0 oracle (${{ matrix.platform }})"),
    group("package"; $extended; [$matrix.packages.include[] | "Package (\(.platform))"]; "Package (${{ matrix.platform }})"),
    group("assets"; $extended; ["Assemble release assets"]; null),
    group("install"; $extended; [$matrix.packages.include[] | "Test installs (\(.platform))"]; "Test installs (${{ matrix.platform }})")
  ] as $groups
| [$groups[] | select(.selected) | .names[]] as $required
| [$groups[] | select(.selected | not) | .placeholder // .names[0]] as $optional
| (if env.GITHUB_WORKFLOW == "Release" then "test / " else "" end) as $prefix
| (env.GITHUB_RUN_ID | tonumber) as $run
| (env.GITHUB_RUN_ATTEMPT | tonumber) as $attempt
| [.[] | .jobs[] | select(.name | startswith($prefix) and . != ($prefix + "ci")) |
    .name |= ltrimstr($prefix)] as $jobs
| [
    ($groups[] | .id as $id | (if .selected then "success" else "skipped" end) as $expected |
      select($needs[$id].result != $expected) |
      "\($id) group: expected \($expected), got \($needs[$id].result)"),
    ($required - [$jobs[].name] | .[] | "Missing required job: \(.)"),
    ($jobs | group_by(.name)[] | select(length > 1) | "Duplicate job: \(.[0].name)"),
    ($jobs[] | . as $job |
      if .run_id != $run or (.run_attempt // $attempt) != $attempt then
        "Job from a different run or attempt: \(.name)"
      elif ($required | index($job.name)) != null then
        select(.status != "completed" or .conclusion != "success" or
          .started_at == null or .completed_at == null or
          (.runner_id | type) != "number" or .runner_id <= 0) |
        "Required job did not complete successfully on a runner: \(.name)"
      elif ($optional | index($job.name)) != null then
        select(.status != "completed" or .conclusion != "skipped") |
        "Unselected job was not skipped: \(.name)"
      else "Unexpected job: \(.name)"
      end)
  ] as $errors
| ($jobs[] | "- \(.name): \(.status) / \(.conclusion)"),
  (if ($errors | length) > 0 then
    ($errors | join("\n") + "\n" | halt_error(1))
  else "All required jobs succeeded in this run attempt."
  end)
