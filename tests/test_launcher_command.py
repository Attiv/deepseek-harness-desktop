from pathlib import Path
import re
import unittest


class LauncherCommandTests(unittest.TestCase):
    def setUp(self):
        self.source = (
            Path(__file__).parents[1] / "src-tauri/src/main.rs"
        ).read_text()

    def test_pnpm_dlx_is_not_given_npx_yes_flag(self):
        self.assertNotIn('"pnpm", "dlx", "-y"', self.source)
        self.assertNotIn("pnpm dlx -y ", self.source)
        self.assertNotIn('"$d/pnpm" dlx -y ', self.source)

    def test_menu_quit_stops_the_owned_backend(self):
        quit_handler = re.search(
            r'"quit"\s*=>\s*\{(?P<body>.*?)\bapp\s*\.\s*exit\s*\(\s*0\s*\)\s*;',
            self.source,
            re.DOTALL,
        )
        self.assertIsNotNone(quit_handler, "quit handler should call app.exit(0)")
        self.assertRegex(
            quit_handler.group("body"),
            r"\bstop_owned_dsh\s*\(\s*app\s*\)\s*;",
        )

    def test_tauri_exit_events_stop_the_owned_backend(self):
        run_callbacks = list(
            re.finditer(
                r"\.run\s*\(\s*\|\s*app\s*,\s*event\s*\|\s*\{(?P<body>.*?)\}\s*\)\s*;",
                self.source,
                re.DOTALL,
            )
        )
        self.assertTrue(run_callbacks, "final run callback should inspect exit events")
        run_callback = run_callbacks[-1]
        exit_cleanup = re.search(
            r"\bif\s+matches!\s*\(\s*event\s*,(?P<events>.*?)\)\s*\{(?P<body>.*?)\}",
            run_callback.group("body"),
            re.DOTALL,
        )
        self.assertIsNotNone(exit_cleanup, "run callback should match exit events")
        self.assertRegex(
            exit_cleanup.group("events"), r"\btauri::RunEvent::ExitRequested\b"
        )
        self.assertRegex(exit_cleanup.group("events"), r"\btauri::RunEvent::Exit\b")
        self.assertRegex(
            exit_cleanup.group("body"),
            r"\bstop_owned_dsh\s*\(\s*app\s*\)\s*;",
        )

    def test_unix_backend_uses_an_owned_process_group(self):
        self.assertRegex(
            self.source,
            r"\bcmd\s*\.\s*process_group\s*\(\s*0\s*\)\s*;",
        )


if __name__ == "__main__":
    unittest.main()
