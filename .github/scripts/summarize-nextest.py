"""Write suite timings and retry results from nextest's JUnit report."""

from html import escape
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

report = Path(sys.argv[1])
print("### Rust test results\n")
if not report.is_file():
    print("Nextest did not produce a JUnit report. See the test step log.")
    sys.exit(0)

root = ET.parse(report).getroot()
print(f"Nextest wall time: {float(root.attrib['time']):.1f}s. Suite times sum test attempts.\n")
print("| Suite | Tests | Time |")
print("| --- | ---: | ---: |")
flaky = []
retry_tags = {"flakyFailure", "flakyError", "rerunFailure", "rerunError"}
for suite in root.findall("testsuite"):
    name = escape(suite.attrib["name"])
    seconds = 0.0
    for test in suite.findall("testcase"):
        seconds += float(test.get("time", "0"))
        seconds += sum(float(attempt.get("time", "0")) for attempt in test if attempt.tag in retry_tags)
        if test.find("flakyFailure") is not None or test.find("flakyError") is not None:
            flaky.append(f"{name} {escape(test.attrib['name'])}")
    print(f"| <code>{name}</code> | {suite.attrib['tests']} | {seconds:.1f}s |")

if flaky:
    print()
    for name in flaky:
        print(f"- **FLAKY** <code>{name}</code>")
else:
    print("\nNo flaky tests reported.")
