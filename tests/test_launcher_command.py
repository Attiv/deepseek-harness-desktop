from pathlib import Path
import unittest


class LauncherCommandTests(unittest.TestCase):
    def test_pnpm_dlx_is_not_given_npx_yes_flag(self):
        source = (Path(__file__).parents[1] / "src-tauri/src/main.rs").read_text()

        self.assertNotIn('"pnpm", "dlx", "-y"', source)
        self.assertNotIn("pnpm dlx -y ", source)
        self.assertNotIn('"$d/pnpm" dlx -y ', source)


if __name__ == "__main__":
    unittest.main()
