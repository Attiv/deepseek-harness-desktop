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

    def test_final_tauri_exit_stops_the_owned_backend(self):
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
        self.assertRegex(exit_cleanup.group("events"), r"\btauri::RunEvent::Exit\b")
        self.assertNotRegex(
            exit_cleanup.group("events"),
            r"\btauri::RunEvent::ExitRequested\b",
            "a cancellable ExitRequested event must not stop the backend",
        )
        self.assertRegex(
            exit_cleanup.group("body"),
            r"\bstop_owned_dsh\s*\(\s*app\s*\)\s*;",
        )

    def test_backend_startup_is_deferred_until_tauri_setup_succeeds(self):
        main = self.source[
            self.source.index("fn main() {"):self.source.index("#[cfg(test)]")
        ]
        plugin = main.index(".plugin(_single)")
        self.assertIn(
            ".manage(DshChild(Mutex::new(None)))",
            main,
            "builder must manage an empty child slot before setup",
        )
        managed_state = main.index(".manage(DshChild(Mutex::new(None)))")
        setup = main.index(".setup(move |app| {")
        menu_events = main.index(".on_menu_event", setup)

        spawn_calls = list(
            re.finditer(r"\bspawn_dsh\s*\(\s*&resolve_dsh_spec\(\)\s*\)", main)
        )
        self.assertEqual(len(spawn_calls), 1, "main should have one deferred backend spawn")
        spawn = spawn_calls[0].start()

        self.assertLess(plugin, setup, "single-instance handling must precede setup")
        self.assertLess(managed_state, setup, "empty child state must exist before setup")
        self.assertLess(setup, spawn, "backend must not start before setup")
        self.assertLess(spawn, menu_events, "backend spawn must remain inside setup")

        shortcut_registration = main.index(".register(shortcut)", setup)
        menu_build = main.index("rebuild_menu(&app.handle())?;", setup)
        main_window = main.index(".visible(false)", setup)
        main_window_build = main.index(".build()?;", main_window)
        for fallible_init in (shortcut_registration, menu_build, main_window_build):
            self.assertLess(
                fallible_init,
                spawn,
                "all fallible initialization must finish before spawning the backend",
            )

        setup_success = main.index("Ok(())", spawn)
        self.assertNotIn(
            "?;",
            main[spawn:setup_success],
            "setup must not return an error after it owns a spawned backend",
        )
        self.assertNotIn("Mutex::new(child.take())", main)

    def test_unix_backend_uses_an_owned_process_group(self):
        self.assertRegex(
            self.source,
            r"\bcmd\s*\.\s*process_group\s*\(\s*0\s*\)\s*;",
        )


if __name__ == "__main__":
    unittest.main()
