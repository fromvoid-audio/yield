#!/usr/bin/env python3
"""Parser/process negatives only; the separate real collector run supplies worker evidence."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import catalog_analysis_evidence as evidence


def contract_case():
    # Synthetic scalars exercise intake rejection, never actual worker qualification.
    current = {**dict.fromkeys(evidence.METRIC_FIELDS[:-1], 0), "saturated": False}
    samples = []
    for index, (name, result, sql_before, sql_after, events) in enumerate(evidence.CASES):
        before = current.copy()
        current["entered"] += 1
        current["terminal_enqueued"] += 1
        current["terminal_ready" if result == "ready" else "terminal_blocked"] += 1
        if result == "blocked_stale": current["terminal_stale"] += 1
        if result == "blocked_cancelled": current["terminal_cancelled"] += 1
        current["total_elapsed_us"] += 10 + index
        current["max_elapsed_us"] = 10 + index
        samples.append({"case":name,"result":result,"before":before,"after":current.copy(),
                        "sql_before":sql_before.copy(),"sql_after":sql_after.copy(),"received_events":events,"wall_us":100})
    return {"schema":"yield.catalog-analysis-case.v1","fixture":"actual-normal-build-catalog-v1","files":2,
            "requests":5,"frame_count":16000,"sample_rate":16000,"samples":samples,"ready_key_reused":True,
            "reset":{**dict.fromkeys(evidence.METRIC_FIELDS[:-1],0),"saturated":False},"restart_sql_retained":True,
            "ui_acknowledged":False,"physical_qualification":False,"slo_claim":False}


def marker(case=None):
    return (evidence.PREFIX + json.dumps(contract_case() if case is None else case)+"\n").encode()


def build_lines(normal=True, features=None):
    library = {"reason":"compiler-artifact","target":{"name":"yield_catalog","kind":["lib"]},
               "features":[],"profile":{"test":not normal}}
    target = {"reason":"compiler-artifact","target":{"name":evidence.TARGET,"kind":["test"],"src_path":str(evidence.ROOT/evidence.FIXTURE)},
              "features":[] if features is None else features,"profile":{"test":True},"executable":"/unused/compiled-test"}
    return (json.dumps(library)+"\n"+json.dumps(target)+"\n").encode()


def contract_report():
    names=("Cargo.toml","Cargo.lock","rust-toolchain.toml","rust-toolchain",".cargo/config.toml",".cargo/config",
           "crates/yield-library/Cargo.toml","cargo_home_config","cargo_home_config_toml","crates/yield-catalog/Cargo.toml")
    source={"head":"a"*40,"tree":"b"*40,"dirty":False,"configuration_sha256":dict.fromkeys(names,"c"*64)}
    return {"schema":"yield.catalog-analysis-report.v1","status":"observed_actual_analysis_cases","source":source,
            "compiled_test":{"target":evidence.TARGET,"test":evidence.TEST,"features":[],"catalog_cfg_test":False,
                             "sha256":"d"*64,"source_head":source["head"],"source_tree":source["tree"]},
            "build":{"command":evidence.BUILD,"exit_status":0,"log_sha256":"e"*64,"log_bytes":100,"timeout_seconds":600},
            "run":{"command":evidence.RUN,"exit_status":0,"log_sha256":"f"*64,"log_bytes":100,"timeout_seconds":60},
            "host":{"system":"Linux","release":"6.1","machine":"x86_64","rustc_version":"rustc 1.93.0 (12345678 2026-01-01)",
                    "cargo_version":"cargo 1.93.0 (12345678 2026-01-01)","cargo_incremental":"0"},
            "case":contract_case(),"limits":evidence.LIMITS,"qualification":"observations_only_no_slo_or_authority"}


class Intake(unittest.TestCase):
    def test_actual_case_schema_marker_and_failed_process(self):
        self.assertEqual(evidence.parse_case(marker(),b"",0),contract_case())
        for out,err,status in ((b"",b"",0),(marker()*2,b"",0),(marker(),marker(),0),
                               (b"noise"+marker(),b"",0),(marker(),b"",1),
                               (marker(),b"x"*evidence.LOG_LIMIT,0)):
            with self.subTest(status=status), self.assertRaises(ValueError): evidence.parse_case(out,err,status)

    def test_malformed_duplicate_nonfinite_depth_and_size(self):
        for payload in (b'{"schema":1,"schema":2}',b'{',b'{} trailing',b'{"x":NaN}',b'{"x":Infinity}',
                        b'['*3000,b'x'*(evidence.CASE_LIMIT+1)):
            with self.subTest(payload=payload[:30]), self.assertRaises(ValueError):
                evidence.parse_case(evidence.PREFIX.encode()+payload+b"\n",b"",0)

    def test_case_result_sql_and_privacy_fields_must_be_exact(self):
        for key,value in (("source_id","private"),("path","/private"),("profile_uuid","private"),
                          ("ui_acknowledged",True),("physical_qualification",True),("slo_claim",True),
                          ("ready_key_reused",False),("restart_sql_retained",False),("requests",6),
                          ("reset",{**contract_case()["reset"],"entered":1})):
            data=contract_case();data[key]=value
            with self.subTest(key=key), self.assertRaises(ValueError): evidence.validate_case(data)
        for mutate in (lambda d: d["samples"].pop(),lambda d: d["samples"].append(d["samples"][0]),
                       lambda d: d["samples"].reverse(),lambda d: d["samples"][1].update(case="ready"),
                       lambda d: d["samples"][0]["sql_after"].update(payload_bytes=731),
                       lambda d: d["samples"][2]["sql_after"].update(retryable_jobs=0),
                       lambda d: d["samples"][3].update(result="ready"),
                       lambda d: d["samples"][0].update(owner="authority")):
            data=contract_case();mutate(data)
            with self.assertRaises(ValueError): evidence.validate_case(data)

    def test_metrics_require_settled_monotonic_exact_delta_and_real_timing_shape(self):
        for mutate in (lambda d: d["samples"][0]["after"].update(saturated=True),
                       lambda d: d["samples"][0]["after"].update(entered=True),
                       lambda d: d["samples"][0]["after"].update(entered=1.0),
                       lambda d: d["samples"][0]["after"].update(terminal_enqueued=0),
                       lambda d: d["samples"][0]["after"].update(terminal_ready=0),
                       lambda d: d["samples"][3]["after"].update(terminal_stale=0),
                       lambda d: d["samples"][4]["after"].update(terminal_cancelled=0),
                       lambda d: d["samples"][1]["before"].update(total_elapsed_us=9),
                       lambda d: d["samples"][0]["after"].update(total_elapsed_us=2**64-1),
                       lambda d: d["samples"][0]["after"].update(max_elapsed_us=9),
                       lambda d: d["samples"][0].update(wall_us=1),
                       lambda d: d["samples"][0].update(wall_us=5_000_001)):
            data=contract_case();mutate(data)
            with self.assertRaises(ValueError): evidence.validate_case(data)

    def test_build_must_prove_normal_catalog_and_unique_exact_integration(self):
        self.assertEqual(evidence.artifact(build_lines()),Path("/unused/compiled-test"))
        for data in (b"",build_lines(normal=False),build_lines(features=["test-support"]),
                     build_lines()+build_lines(),b"x"*(evidence.BUILD_LIMIT+1)):
            with self.assertRaises(ValueError): evidence.artifact(data)

    def test_source_dirty_and_semantic_override_are_rejected(self):
        with patch.object(evidence.runtime,"capture",return_value="?? untracked-private"), self.assertRaisesRegex(ValueError,"dirty_checkout"):
            evidence.binding()
        with self.assertRaises(ValueError): evidence.runtime.build_configuration({"RUSTFLAGS":"private-override"})

    def test_failure_never_publishes_or_replaces_previous_artifact(self):
        # A valid shape alone is not collection provenance. Failed collect cannot publish.
        with tempfile.TemporaryDirectory() as directory:
            output=Path(directory)/"report.json"
            output.write_bytes(b"existing")
            with patch.object(evidence,"validate_report"), self.assertRaises(FileExistsError): evidence.publish(output,b"new")
            self.assertEqual(output.read_bytes(),b"existing")
            self.assertEqual(sorted(p.name for p in Path(directory).iterdir()),["report.json"])

    def test_report_missing_source_lock_normal_build_binary_or_toolchain_is_proof_gap(self):
        report=contract_report()
        self.assertEqual(evidence.validate_report(json.dumps(report).encode()),report)
        for mutate in (lambda d: d["source"].update(dirty=True),
                       lambda d: d["source"]["configuration_sha256"].update({"Cargo.lock":None}),
                       lambda d: d["compiled_test"].update(source_head="0"*40),
                       lambda d: d["compiled_test"].update(catalog_cfg_test=True),
                       lambda d: d["compiled_test"].update(sha256="missing"),
                       lambda d: d["run"].update(exit_status=1),
                       lambda d: d["build"].update(log_bytes=evidence.BUILD_LIMIT+1),
                       lambda d: d["host"].update(rustc_version="rustc missing"),
                       lambda d: d.update(qualification="physical_success")):
            data=copy.deepcopy(report);mutate(data)
            with self.assertRaises(ValueError): evidence.validate_report(json.dumps(data).encode())

    def test_whole_collector_deadline_restores_alarm_and_does_not_publish(self):
        with patch.object(evidence,"collect_bounded",side_effect=RuntimeError("gap")), \
             patch.object(evidence.signal,"signal",return_value="previous") as handler, \
             patch.object(evidence.signal,"setitimer") as timer:
            with self.assertRaises(RuntimeError): evidence.collect()
            self.assertEqual(timer.call_args_list[-1].args,(evidence.signal.ITIMER_REAL,0))
            self.assertEqual(handler.call_args_list[-1].args,(evidence.signal.SIGALRM,"previous"))


class Processes(unittest.TestCase):
    def test_real_nonzero_process_with_valid_marker_cannot_pass(self):
        result=evidence.runtime.run_bounded([sys.executable,"-c","import sys; print('YIELD_CATALOG_ANALYSIS_EVIDENCE={}'); sys.exit(7)"],timeout=5,output_limit=4096)
        self.assertEqual(result.returncode,7)
        with self.assertRaisesRegex(ValueError,"fixture_process_failed"):
            evidence.parse_case(result.stdout.encode(),result.stderr.encode(),result.returncode)

    def test_real_timeout_and_output_limit_reap_only_owned_process(self):
        for code,timeout,limit in (("import time; time.sleep(10)",0.1,4096),
                                   ("import sys; sys.stdout.write('x'*100000); sys.stdout.flush()",5,4096)):
            with self.subTest(timeout=timeout), self.assertRaises(RuntimeError):
                evidence.runtime.run_bounded([sys.executable,"-c",code],timeout=timeout,output_limit=limit)


if __name__ == "__main__": unittest.main()
