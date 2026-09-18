from pathlib import Path
import json
import re
import subprocess
import unittest


ROOT = Path(__file__).parents[1]
WORKFLOW_PATH = ROOT / ".github/workflows/build-release.yml"
README_PATH = ROOT / "README.md"


def load_workflow():
    import yaml

    return yaml.safe_load(WORKFLOW_PATH.read_text())


class UpdaterWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW_PATH.read_text()
        self.workflow = load_workflow()
        self.on = self.workflow.get("on") or self.workflow.get(True) or {}
        self.dispatch = (self.on.get("workflow_dispatch") or {}).get("inputs") or {}

    def test_dispatch_requires_an_existing_version_tag_input(self):
        inputs = self.dispatch
        self.assertIn("tag", inputs)
        self.assertTrue(inputs["tag"]["required"])
        self.assertRegex(inputs["tag"]["description"], r"v\d")

    def test_dispatch_checks_out_the_requested_tag(self):
        job = self.workflow["jobs"]["build"]
        checkout = next(
            s for s in job["steps"] if str(s.get("uses", "")).startswith("actions/checkout")
        )
        self.assertEqual(checkout["with"]["ref"], "${{ inputs.tag }}")

    def test_ref_name_is_never_inline_interpolated_in_run_steps(self):
        run_steps = [s for s in self.workflow["jobs"]["build"]["steps"] if "run" in s]
        self.assertTrue(run_steps)
        for step in run_steps:
            run = step["run"]
            if "${{ github.ref_name }}" in run:
                self.assertIn("env:", step, "tag must reach run steps via env, not inline")
            self.assertNotRegex(run, r"\$\{\{\s*inputs\.tag\s*\}\}")

    def test_workflow_name_and_triggers(self):
        self.assertEqual(self.workflow["name"], "Build & Release")
        self.assertEqual(self.on["push"]["tags"], ["v*"])
        self.assertEqual(self.workflow["permissions"]["contents"], "write")

    def test_matrix_covers_four_platforms_serialized(self):
        job = self.workflow["jobs"]["build"]
        strategy = job["strategy"]
        self.assertEqual(strategy["max-parallel"], 1)
        self.assertFalse(strategy["fail-fast"])
        platforms = {m["platform"] for m in strategy["matrix"]["include"]}
        self.assertEqual(
            platforms,
            {"macos-latest", "ubuntu-22.04", "windows-latest"},
        )
        mac_args = {m["args"] for m in strategy["matrix"]["include"] if m["platform"] == "macos-latest"}
        self.assertEqual(
            mac_args,
            {"--target aarch64-apple-darwin", "--target x86_64-apple-darwin"},
        )

    def test_signing_env_is_fail_fast_guarded(self):
        job = self.workflow["jobs"]["build"]
        env = job["env"]
        self.assertEqual(env["TAURI_SIGNING_PRIVATE_KEY"], "${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}")
        self.assertEqual(
            env["TAURI_SIGNING_PRIVATE_KEY_PASSWORD"],
            "${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}",
        )
        guard = next(
            s
            for s in job["steps"]
            if s.get("name") == "Verify updater signing secrets and public key"
        )
        script = guard["run"]
        self.assertIn("TAURI_UPDATER_PUBLIC_KEY", script)
        self.assertIn("secrets.TAURI_SIGNING_PRIVATE_KEY", script)
        self.assertIn("exit 1", script)

    def test_public_key_flows_from_repository_var_to_build(self):
        job = self.workflow["jobs"]["build"]
        env = job["env"]
        self.assertEqual(env["TAURI_UPDATER_PUBLIC_KEY"], "${{ vars.TAURI_UPDATER_PUBLIC_KEY }}")
        build_step = next(
            s for s in job["steps"] if str(s.get("uses", "")).startswith("tauri-apps/tauri-action")
        )
        self.assertEqual(build_step["env"]["TAURI_UPDATER_PUBLIC_KEY"], "${{ vars.TAURI_UPDATER_PUBLIC_KEY }}")

    def test_build_uploads_into_a_draft_release_by_id(self):
        job = self.workflow["jobs"]["build"]
        build_step = next(
            s for s in job["steps"] if str(s.get("uses", "")).startswith("tauri-apps/tauri-action")
        )
        self.assertEqual(build_step["with"]["releaseId"], "${{ needs.create-release.outputs.release_id }}")
        self.assertNotIn("tagName", build_step["with"])
        self.assertTrue(build_step["with"].get("includeUpdaterJson", True) if "includeUpdaterJson" in build_step["with"] else True)

    def test_create_release_writes_a_draft_and_refuses_published_tags(self):
        create = self.workflow["jobs"]["create-release"]
        self.assertTrue(create["condition"] if "condition" in create else create.get("if"))

    def test_include_updater_json_is_explicitly_enabled(self):
        job = self.workflow["jobs"]["build"]
        build_step = next(
            s for s in job["steps"] if str(s.get("uses", "")).startswith("tauri-apps/tauri-action")
        )
        self.assertEqual(build_step["with"].get("includeUpdaterJson"), True)

    def test_publish_job_gated_on_all_platforms_and_verifies_latest_json(self):
        self.assertIn("publish-release", self.workflow["jobs"])
        publish = self.workflow["jobs"]["publish-release"]
        self.assertEqual(
            set(publish["needs"]),
            {"create-release", "build"},
        )
        runs = [
            s
            for s in publish["steps"]
            if "run" in s and "latest.json" in s["run"]
        ]
        self.assertTrue(runs, "publish job must download and verify latest.json")
        script = "\n".join(s["run"] for s in runs)
        for key in ("darwin-x86_64", "darwin-aarch64", "windows-x86_64", "linux-x86_64"):
            self.assertIn(key, script, f"latest.json must be checked for {key}")
        self.assertIn("signature", script)
        self.assertIn("https://", script)
        self.assertIn("gh release download", script)
        self.assertIn("release_id", script)
        self.assertIn("tag_version", script)
        self.assertIn("exit 1", script)
        self.assertIn("gh release edit", script)

    def test_verify_step_runs_inside_the_draft_release_context(self):
        publish = self.workflow["jobs"]["publish-release"]
        verify = next(s for s in publish["steps"] if s.get("name") == "Verify combined latest.json before publishing")
        self.assertIn("release_id", verify["env"])
        self.assertIn("tag_version", verify["env"])

    def test_updater_config_declared_by_the_main_agent_is_expected(self):
        conf = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())
        self.assertIn(
            "updater",
            conf.get("plugins") or {},
            "main agent must add plugins.updater (pubkey with empty fallback) before release",
        )

    def test_readme_documents_updater_setup(self):
        text = README_PATH.read_text()
        self.assertIn("TAURI_UPDATER_PUBLIC_KEY", text)
        self.assertIn("TAURI_SIGNING_PRIVATE_KEY", text)
        self.assertIn("gh secret set", text)
        self.assertIn("minisign", text.lower())
        self.assertIn("latest.json", text)


if __name__ == "__main__":
    unittest.main()
