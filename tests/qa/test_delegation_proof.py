"""The proof harness itself is hermetic: these checks never start an installed agent."""

import json
import copy
from pathlib import Path
import sys
import tempfile
import unittest
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import delegation_proof as proof


class DelegationProofTests(unittest.TestCase):
    def test_fixture_export_is_a_versioned_gate_projection_without_transcript_prose(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)/"private"
            target = Path(temporary)/"public"
            source.mkdir()
            marker = "incidental-personal-or-model-prose-must-not-publish"
            entries = [
                {"dir":"stdin","at":100,"frame":{"method":"turn/start","params":{"input":[{"text":marker}]}}},
                {"dir":"stdout","at":101,"frame":{"method":"item/agentMessage/delta","params":{"delta":marker}}},
                {"dir":"stderr","at":102,"frame":{"raw":marker}},
                {"dir":"stdout","at":103,"frame":{"result":{"config":{"features":{"multi_agent":False,"multi_agent_v2":False},"instructions":marker,"mcp_servers":{}}}}},
            ]
            (source/"codex.jsonl").write_text("\n".join(json.dumps(entry) for entry in entries))
            (source/"codex-results.json").write_text(json.dumps({"provider":"codex","passed":False,"error":marker,"model_transport":marker}))
            proof.export_proof_fixtures(source,target)
            self.assertFalse(list(target.glob("*.jsonl")))
            saved = target/"codex.proof.json"
            payload = json.loads(saved.read_text())
            self.assertEqual(payload["fixture_version"],1)
            self.assertEqual(payload["evidence_kind"],"codex")
            self.assertEqual(payload["provider"],"codex")
            self.assertEqual(len(payload["frames"]),1)
            self.assertFalse(payload["frames"][0]["frame"]["result"]["config"]["features"]["multi_agent"])
            self.assertNotIn(marker,saved.read_text())
            result = (target/"codex-results.json").read_text()
            self.assertNotIn(marker,result)
            self.assertTrue(json.loads(result)["error_present"])

    def test_agy_recorded_attestation_requires_exact_arguments_identity_and_deadline(self):
        fixtures = Path(__file__).resolve().parents[1]/"fixtures/real/delegation/route-proof"
        calls = [entry for entry in json.loads((fixtures/"agy-mcp.proof.json").read_text())["frames"] if entry.get("event")=="call"]
        frames = [(entry["at"],entry["frame"]) for entry in json.loads((fixtures/"agy.proof.json").read_text())["frames"] if entry.get("dir")=="stdout"]
        self.assertTrue(proof.agy_attested(calls,frames))
        for field in ["arguments","progressToken","conversation"]:
            bad = copy.deepcopy(calls)
            if field=="arguments":
                bad[0]["arguments"]={"message":"forged"}
            elif field=="progressToken":
                bad[0]["meta"]["progressToken"]="forged:9999"
            else:
                bad[0]["meta"]["antigravity.google/conversation_id"]="forged"
            self.assertFalse(proof.agy_attested(bad,frames),field)
        late = [(calls[0]["at"]+3,frame) for _,frame in frames]
        self.assertFalse(proof.agy_attested(calls,late))

    def test_codex_recorded_false_feature_echo_does_not_hide_a_started_worker(self):
        fixtures = Path(__file__).resolve().parents[1]/"fixtures/real/delegation/route-proof"
        entries = json.loads((fixtures/"codex.proof.json").read_text())["frames"]
        features = next(entry["frame"]["result"]["config"]["features"] for entry in entries
                        if "config" in entry.get("frame",{}).get("result",{}))
        self.assertFalse(features["multi_agent"])
        self.assertFalse(features["multi_agent_v2"])
        started = [entry for entry in entries if entry.get("frame",{}).get("method")=="item/started"
                   and entry["frame"]["params"]["item"].get("type")=="subAgentActivity"]
        self.assertTrue(started)
        self.assertFalse(json.loads((fixtures/"codex-results.json").read_text())["G-X2"])

    def test_scan_ignores_inputs_and_symlinks_but_finds_provider_persistence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker = "synthetic-proof-marker"
            source = root / "overlay.json"
            source.write_text(marker)
            (root / "rollout.jsonl").write_text(marker)
            (root / "link").symlink_to(source)
            self.assertEqual(proof.scan_canary(root, marker, excluded=[source]), ["rollout.jsonl"])

    def test_canary_scan_finds_a_marker_across_its_bounded_read_boundary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root/"rollout").write_bytes(b"x"*65530+b"synthetic-proof-marker"+b"y"*70000)
            self.assertEqual(proof.scan_canary(root,"synthetic-proof-marker"),["rollout"])

    def test_isolated_environment_has_no_ambient_credentials(self):
        with tempfile.TemporaryDirectory() as temporary:
            env = proof.environment(Path(temporary))
            self.assertEqual(env["HOME"], temporary)
            self.assertFalse(any("KEY" in key or "TOKEN" in key or "CAP" in key for key in env))
            self.assertNotIn("CLAUDECODE", env)
            self.assertNotIn("CODEX_HOME", env)

    def test_local_model_observes_new_additional_tools_wire_and_native_call(self):
        with proof.ModelFixture() as model:
            model.native_probe = True
            body = {"model": "fixture", "input": [{"type": "additional_tools", "role": "developer",
                    "tools": [{"type": "namespace", "name": "collaboration", "tools": [
                        {"type": "function", "name": "spawn_agent"}]}]}]}
            request = urllib.request.Request(model.url + "/v1/responses", data=json.dumps(body).encode(),
                                             headers={"Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=3) as response:
                events = response.read().decode()
            self.assertEqual(model.tools, [["spawn_agent"]])
            self.assertIn('"namespace": "collaboration"', events)
            self.assertIn('"call_id": "call_proof"', events)

    def test_wire_masks_all_capability_generations_before_writing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            env = proof.environment(home)
            env["PROOF_LOG"] = str(root / "mcp.jsonl")
            env["PROOF_CAP"] = "atdc_" + "a" * 64
            wire = proof.Wire([sys.executable, str(proof.HERE), "--mcp-child"], root, env,
                              root / "wire.jsonl", "first-generation")
            try:
                response = wire.call("initialize", {"protocolVersion": "2025-06-18", "canary": env["PROOF_CAP"]})
                self.assertIn("result", response)
            finally:
                wire.close()
            recorded = (root / "wire.jsonl").read_text()
            self.assertNotIn(env["PROOF_CAP"], recorded)
            self.assertIn("<CAPABILITY_CANARY>", recorded)


if __name__ == "__main__":
    unittest.main()
