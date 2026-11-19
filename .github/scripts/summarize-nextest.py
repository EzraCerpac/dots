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
print(f"Nextest wall time: {float(root.attrib['time']):.1f}s. Suite times sum test runtimes.\n")
print("| Suite | Tests | Time |")
print("| --- | ---: | ---: |")
flaky = []
for suite in root.findall("testsuite"):
    name = escape(suite.attrib["name"])
    print(f"| <code>{name}</code> | {suite.attrib['tests']} | {float(suite.attrib['time']):.1f}s |")
    for test in suite.findall("testcase"):
        if test.find("flakyFailure") is not None or test.find("flakyError") is not None:
            flaky.append(f"{name} {escape(test.attrib['name'])}")

if flaky:
    print()
    for name in flaky:
        print(f"- **FLAKY** <code>{name}</code>")
else:
    print("\nNo flaky tests reported.")
