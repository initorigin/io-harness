"""One Windows round, every Windows-only sabotage arm.

A sabotage that fails nothing is a finding about the test, and a sabotage that
fails to COMPILE is a false kill — so each arm reports which of the three it was
and the named test is run alone rather than the suite.
"""
import subprocess, sys, pathlib

ARMS = [
    ("S6  run_contained reports the job object",
     "src/sandbox/windows.rs",
     "            backend: Backend::WindowsAppContainer,",
     "            backend: Backend::WindowsJobObject,",
     "sandbox::windows::tests::what_the_container_actually_permits_on_this_host"),
    ("S7  a failed Full grant declines silently instead of fatally",
     "src/sandbox/windows.rs",
     "            if g.grant == Grant::ReadExecute || g.grant == Grant::Traverse {",
     "            if true {",
     "sandbox::windows::tests::what_the_container_actually_permits_on_this_host"),
    ("S8  the container is selected unconditionally",
     "src/sandbox.rs",
     "            if config.access_confinement && config.mode != ExecMode::FullAccess {",
     "            if config.mode != ExecMode::FullAccess {",
     "sandbox::windows::tests::the_container_is_chosen_only_when_the_caller_asks_for_it"),
    ("S9  the grant memo forgets which container it granted to",
     "src/sandbox/appcontainer.rs",
     "        let memo_key = (path.to_path_buf(), access, reach, sid_key(sid));",
     "        let memo_key = (path.to_path_buf(), access, reach, Vec::new());",
     "sandbox::appcontainer::tests::a_grant_to_one_container_is_not_a_grant_to_another"),
    ("S10 the capability array is dropped at the spawn",
     "src/sandbox/appcontainer.rs",
     "                        granted[0].Attributes = 0x0000_0004;\n                        1u32",
     "                        granted[0].Attributes = 0x0000_0004;\n                        0u32",
     "sandbox::appcontainer::tests::a_payload_has_no_route_off_the_machine"),
    # S11's first arm was inert rather than a finding: it wrote the proxy into
    # `stderr`, and the test reads `stdout`. An arm that cannot express the defect
    # is not evidence the guard is load-bearing. This one hands the command line
    # the proxy the way a regression actually would.
    ("S11 a contained command is handed the run's proxy",
     "src/sandbox/windows.rs",
     "        let cmdline = super::command_line(spec.argv);",
     "        let cmdline = match spec.proxy {\n            Some(p) => format!(\"cmd.exe /c set HTTP_PROXY=http://{p}&& {}\", super::command_line(spec.argv)),\n            None => super::command_line(spec.argv),\n        };",
     "sandbox::windows::tests::a_contained_command_is_not_pointed_at_a_proxy_it_cannot_reach"),
    ("S7b a failed Full grant declines silently instead of refusing",
     "src/sandbox/windows.rs",
     "            if g.grant == Grant::ReadExecute || g.grant == Grant::Traverse {",
     "            if true {",
     "sandbox::windows::tests::a_boundary_that_cannot_be_applied_refuses_rather_than_running_uncontained"),
    ("S12 the container claims it can scope egress per host",
     "src/sandbox.rs",
     "            Backend::WindowsAppContainer | Backend::WindowsJobObject | Backend::PortableFloor => {\n                false\n            }",
     "            Backend::WindowsJobObject | Backend::PortableFloor => false,\n            Backend::WindowsAppContainer => true,",
     "sandbox::tests::backend_claims"),
]

def revert():
    subprocess.run(["git", "checkout", "--", "src/"], check=True)

report = []
for name, path, old, new, test in ARMS:
    p = pathlib.Path(path)
    s = p.read_text(encoding="utf-8")
    if old not in s:
        report.append(f"{name}: ANCHOR MISSING — the arm never applied, which is not evidence")
        continue
    p.write_text(s.replace(old, new, 1), encoding="utf-8")
    r = subprocess.run(
        ["cargo", "test", "--all-features", "--lib", "--", "--exact", test],
        capture_output=True, text=True,
    )
    out = r.stdout + r.stderr
    revert()
    if "error[" in out or "error: could not compile" in out:
        report.append(f"{name}: DID NOT COMPILE — a false kill, rewrite the arm")
    elif r.returncode != 0:
        report.append(f"{name}: killed {test}")
    else:
        report.append(f"{name}: **SURVIVED** — {test} does not construct the case this guard exists for")

print("\n".join(report))
pathlib.Path("diagnostics/sabotage.txt").write_text("\n".join(report), encoding="utf-8")
