require "fileutils"
require "json"
require "open3"
require "tmpdir"
require "yaml"

jobs = YAML.load_file(".github/workflows/rust.yml").fetch("jobs")
aggregate = jobs.fetch("ci")
raise "ci must depend on every other CI job" unless aggregate.fetch("needs").sort == (jobs.keys - ["ci"]).sort
step = aggregate.fetch("steps").find { |entry| entry["name"] == "Summarize and check results" }
caller = YAML.load_file(".github/workflows/release.yml").fetch("jobs").fetch("test")
[aggregate, caller].each do |job|
  raise "ci needs Actions read access" unless job.dig("permissions", "actions") == "read"
end

# Evidence from release binaries should still run when the test jobs fail.
{ "package" => [], "assets" => ["package"], "install" => ["assets"] }.each do |id, artifacts|
  job = jobs.fetch(id)
  prerequisites = ["changes", *artifacts]
  conditions = ["!cancelled()", "needs.changes.result == 'success'",
                "needs.changes.outputs.code == 'true'", "needs.changes.outputs.full == 'true'"]
  conditions.concat(artifacts.map { |input| "needs.#{input}.result == 'success'" })
  raise "#{id} has unexpected prerequisites" unless Array(job.fetch("needs")) == prerequisites
  raise "#{id} has an unexpected guard" unless job.fetch("if").split.join(" ") == "${{ #{conditions.join(' && ')} }}"
end
install_steps = jobs.fetch("install").fetch("steps")
candidate = install_steps.find { |entry| entry["id"] == "candidate" }
raise "candidate must download the release archives" unless candidate&.dig("with", "name") == "release-archives"
{
  "Run install.sh in a fresh container with the release archive" => "linux-amd64",
  "Exercise shipped scaffold examples with native Homebrew" => "darwin-arm64"
}.each do |name, platform|
  guard = "${{ !cancelled() && steps.candidate.outcome == 'success' && matrix.platform == '#{platform}' }}"
  raise "#{name} has an unexpected guard" unless install_steps.find { |entry| entry["name"] == name }&.fetch("if") == guard
end
puts "Packaging and install evidence depends on classification and artifacts"

def job_rows(jobs, needs, matrix, prefix, run, attempt)
  needs.flat_map do |id, need|
    job = jobs.fetch(id)
    template = job.fetch("name")
    skipped = need.fetch("result") == "skipped"
    strategy = job.dig("strategy", "matrix")
    variants = if skipped || !strategy
                 [nil]
               elsif strategy.is_a?(Hash)
                 strategy.fetch("distro").map { |distro| { "distro" => distro } }
               else
                 matrix.fetch(strategy[/fromJSON\(needs\.changes\.outputs\.matrix\)\.(\w+)/, 1]).fetch("include")
               end
    variants.map do |variant|
      name = if variant
               template.gsub(/\$\{\{ matrix\.(\w+) \}\}/) { variant.fetch($1).to_s }
             else
               template
             end
      { "name" => prefix + name, "run_id" => run, "run_attempt" => attempt,
        "status" => "completed", "conclusion" => need.fetch("result"),
        "started_at" => "2026-01-01T00:00:00Z", "completed_at" => "2026-01-01T00:00:01Z",
        "runner_id" => skipped ? nil : 73 }
    end
  end
end

Dir.mktmpdir do |dir|
  output = File.join(dir, "outputs")
  classifier = File.expand_path(".github/scripts/classify-changes")
  raise "classifier failed" unless system({ "REF_TYPE" => "tag", "EVENT" => "push", "GITHUB_OUTPUT" => output },
                                          "bash", classifier)
  full_matrix = JSON.parse(File.readlines(output).find { |line| line.start_with?("matrix=") }.split("=", 2).last)
  short_matrix = full_matrix.merge(full_matrix.reject { |key, _| key == "linux" }.transform_values { { "include" => [] } })

  Dir.mktmpdir do |repo|
    git = lambda do |*args|
      stdout, stderr, status = Open3.capture3("git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false",
                                             "-c", "user.name=CI", "-c", "user.email=ci@example.invalid", *args, chdir: repo)
      raise stderr unless status.success?
      stdout.strip
    end
    cases = {
      %w[src/init/README.md] => %w[code=true full=false],
      %w[skills/dots/SKILL.md] => %w[code=true full=false],
      %w[docs/contract.md] => %w[code=false full=false],
      %w[build.rs install.sh] => %w[code=true full=true]
    }
    git.call("init", "-q")
    cases.keys.flatten.each do |path|
      file = File.join(repo, path)
      FileUtils.mkdir_p(File.dirname(file))
      File.write(file, "initial\n")
    end
    git.call("add", ".")
    git.call("commit", "-qm", "base")
    base = git.call("rev-parse", "HEAD")
    cases.each do |paths, expected|
      git.call("reset", "--hard", base)
      paths.each { |path| File.write(File.join(repo, path), "modified\n") }
      git.call("add", "--", *paths)
      git.call("commit", "-qm", "change")
      File.truncate(output, 0)
      env = { "REF_TYPE" => "branch", "EVENT" => "pull_request", "BASE" => base,
              "HEAD_SHA" => git.call("rev-parse", "HEAD"), "GITHUB_OUTPUT" => output }
      raise "classifier failed for #{paths.join(', ')}" unless system(env, "bash", classifier, chdir: repo)
      raise "wrong classifier route for #{paths.join(', ')}" unless File.readlines(output, chomp: true).first(2) == expected
      puts "Classifier: #{paths.join(', ')} → #{expected.join(' ')}"
    end
  end

  run = 45_123
  attempt = 4
  path = File.join(dir, "gh")
  File.write(path, <<~SH)
    #!/bin/sh
    test "$#" -eq 4 &&
      test "$*" = 'api --paginate --slurp repos/example/dots/actions/runs/#{run}/attempts/#{attempt}/jobs?per_page=100' &&
      cat "$JOBS_RESPONSE"
  SH
  File.chmod(0755, path)
  env = {
    "PATH" => "#{dir}:#{ENV.fetch('PATH')}", "JOBS_RESPONSE" => File.join(dir, "jobs.json"),
    "GITHUB_REPOSITORY" => "example/dots", "GITHUB_RUN_ID" => run.to_s,
    "GITHUB_RUN_ATTEMPT" => attempt.to_s, "GITHUB_STEP_SUMMARY" => File.join(dir, "summary")
  }

  [
    ["ordinary PR", true, false, "CI", 9],
    ["Markdown PR", false, false, "CI", 2],
    ["Markdown main push", false, true, "CI", 2],
    ["full CI", true, true, "CI", 25],
    ["reusable Release", true, true, "Release", 25]
  ].each do |label, code, full, workflow, count|
    matrix = full ? full_matrix : short_matrix
    selected = %w[changes secrets]
    selected += %w[lint native distros] if code
    selected += full ? %w[mac-build mac-tests oracle package assets install] : %w[mac-pr] if code
    needs = aggregate.fetch("needs").to_h { |id| [id, { "result" => selected.include?(id) ? "success" : "skipped" }] }
    needs.fetch("changes")["outputs"] = { "code" => code.to_s, "full" => full.to_s, "matrix" => JSON.generate(matrix) }
    prefix = workflow == "Release" ? "test / " : ""
    rows = job_rows(jobs, needs, matrix, prefix, run, attempt)
    raise "#{label}: wrong required count" unless rows.count { |row| row["conclusion"] == "success" } == count
    rows << { "name" => prefix + "ci", "status" => "in_progress" }
    rows << { "name" => "release", "status" => "queued" } if workflow == "Release"

    check = lambda do |description, payload, issue|
      # Exercise the workflow's invocation with multiple API pages.
      File.write(env.fetch("JOBS_RESPONSE"), JSON.generate(payload.each_slice(3).map { |page| { "jobs" => page } }))
      _, stderr, status = Open3.capture3(env.merge("RESULTS" => JSON.generate(needs), "GITHUB_WORKFLOW" => workflow),
                                       "bash", "-eo", "pipefail", "-c", step.fetch("run"))
      pass = issue ? !status.success? && stderr.include?(issue) : status.success?
      raise "#{label}, #{description}: #{stderr}" unless pass
      puts "#{label}: #{description} passed"
    end
    check.call("all selected jobs succeeded", rows, nil)
    check.call("omitted unselected jobs", rows.reject { |row| row["conclusion"] == "skipped" }, nil)
    next unless full && code

    failed_test = prefix + "Rust tests (aarch64-apple-darwin shard 1/2)"
    failed_rows = rows.map { |row| row["name"] == failed_test ? row.merge("conclusion" => "failure") : row }
    check.call("failed test with successful packaging and installs", failed_rows,
               "Required job did not complete successfully on a runner: Rust tests (aarch64-apple-darwin shard 1/2)")
    needs.fetch("mac-tests")["result"] = "failure"
    check.call("failed test group with successful packaging and installs", failed_rows,
               "mac-tests group: expected success, got failure")
    needs.fetch("mac-tests")["result"] = "success"

    leg = prefix + "Package (linux-arm64)"
    without_leg = rows.reject { |row| row["name"] == leg }
    selected_row = rows.find { |row| row["name"] == leg }
    rejection = "Required job did not complete successfully on a runner"
    check.call("cancelled leg", without_leg + [selected_row.merge("conclusion" => "cancelled")], rejection)
    check.call("never-assigned leg", without_leg + [selected_row.merge("runner_id" => 0)], rejection)
    check.call("never-started leg", without_leg + [selected_row.merge("status" => "queued", "conclusion" => nil, "started_at" => nil)], rejection)
    check.call("skipped selected leg", without_leg + [selected_row.merge("conclusion" => "skipped")], rejection)
    check.call("absent leg", without_leg, "Missing required job: Package (linux-arm64)")
    check.call("leg from earlier attempt", without_leg + [selected_row.merge("run_attempt" => attempt - 1)], "different run or attempt")
    check.call("duplicate leg", rows + [selected_row], "Duplicate job")
    optional = rows.find { |row| row["conclusion"] == "skipped" }
    check.call("cancelled unselected job", rows.map { |row| row == optional ? row.merge("conclusion" => "cancelled") : row },
               "Unselected job was not skipped")
    needs.fetch("package")["result"] = "failure"
    check.call("failed package group", rows, "package group: expected success, got failure")
  end
end
