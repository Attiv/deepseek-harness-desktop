from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest


SOURCE_PATH = Path(__file__).parents[1] / "src-tauri/src/main.rs"


class LauncherCommandTests(unittest.TestCase):
    def setUp(self):
        self.source = SOURCE_PATH.read_text()

    def test_desktop_updater_is_registered_without_webview_permissions(self):
        import json

        self.assertIn('.plugin(tauri_plugin_updater::Builder::new().build())', self.source)
        permissions = json.loads(
            (SOURCE_PATH.parents[1] / 'capabilities/default.json').read_text()
        )['permissions']
        self.assertFalse(any(item.startswith('updater:') for item in permissions))

    def test_pnpm_dlx_is_not_given_npx_yes_flag(self):
        self.assertNotIn('"pnpm", "dlx", "-y"', self.source)
        self.assertNotIn("pnpm dlx -y ", self.source)
        self.assertNotIn('"$d/pnpm" dlx -y ', self.source)

    def test_backend_starts_without_stdin(self):
        """无 stdin 是这些反交互措施的前提:任何提问都读不到答案,只会挂住。"""
        self.assertRegex(
            self.source,
            r"\bcmd\s*\.\s*stdin\s*\(\s*Stdio::null\(\)\s*\)",
        )

    def test_package_manager_download_prompts_are_disabled(self):
        """corepack 在需要下载新版 pnpm/npm 时会问 [Y/n] 并阻塞 —— 必须预先关掉。

        这正是「平时能用、一到升级就卡死」的成因。
        """
        self.assertRegex(
            self.source,
            r'\.env\s*\(\s*"COREPACK_ENABLE_DOWNLOAD_PROMPT"\s*,\s*"0"\s*\)',
        )
        self.assertRegex(
            self.source,
            r'\.env\s*\(\s*"npm_config_yes"\s*,\s*"true"\s*\)',
        )

    def test_ci_is_not_forced_on_the_backend(self):
        """CI=1 会被整个后端及其中执行的每条命令继承,不能用它来消提示。"""
        self.assertNotRegex(self.source, r'\.env\s*\(\s*"CI"\s*,')

    def test_every_npx_fallback_is_non_interactive(self):
        """npx 装包前会问 "Ok to proceed? (y)",漏一个 -y 就是一次挂死。"""
        self.assertNotRegex(
            self.source,
            r"\bnpx\s+\{spec\}",
            "npx 回退不能不带 -y",
        )
        self.assertEqual(
            len(re.findall(r"\bnpx -y \{spec\} web --no-open", self.source)),
            3,
            "Windows / macOS / Linux 三条回退路径都要带 -y",
        )

    def test_windows_probes_pnpm_before_choosing_a_runner(self):
        """Windows 用 where 探测后分支,而不是 `pnpm ... || npx ...`。

        `||` 会在 pnpm 存在但 dsh 自身崩溃时也回退,白拉一次 220 MB 重装。
        """
        self.assertIn("where pnpm >nul 2>nul & if errorlevel 1", self.source)
        self.assertNotIn("pnpm dlx {spec} web --no-open || npx", self.source)

    def test_windows_command_line_bypasses_rust_argument_escaping(self):
        """cmd 的 `&`/`(` 是语法,被 Rust 转义成字面量就失效了,必须走 raw_arg。"""
        self.assertRegex(
            self.source,
            r'\.arg\s*\(\s*"/C"\s*\)\s*\.raw_arg\s*\(',
        )

    def test_platform_branches_are_compile_time_gated(self):
        """raw_arg 只在 Windows 存在,运行时 cfg! 的 if-else 会让 macOS 编译失败。"""
        spawn = self.source[
            self.source.index("fn spawn_dsh("):self.source.index("fn try_reap_child(")
        ]
        self.assertIn('#[cfg(target_os = "windows")]', spawn)
        self.assertIn('#[cfg(target_os = "macos")]', spawn)
        self.assertIn(
            '#[cfg(not(any(target_os = "windows", target_os = "macos")))]',
            spawn,
        )
        self.assertNotIn('if cfg!(target_os = "windows")', spawn)

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

    def test_default_channel_is_next_not_latest(self):
        """跟 latest 会让壳长期落后于上游预览版,表现为「客户端卡住/不工作」,
        用户只能退回终端手跑 @next。默认必须是 next。"""
        self.assertRegex(self.source, r'const DEFAULT_CHANNEL:\s*&str\s*=\s*"next"')
        self.assertNotRegex(
            self.source,
            r'const DEFAULT_CHANNEL:\s*&str\s*=\s*"latest"',
            "默认频道不能是 latest",
        )

    def test_menu_offers_channel_switch(self):
        """菜单里要能切 next / latest,并反映当前生效值(勾选态)。"""
        self.assertIn('SubmenuBuilder::new(app, "DSH 频道")', self.source)
        self.assertIn('("next", "next(预览版,默认)")', self.source)
        self.assertIn('("latest", "latest(稳定版)")', self.source)
        self.assertRegex(
            self.source,
            r'CheckMenuItemBuilder::with_id\(',
            "频道项应使用可勾选菜单项",
        )
        self.assertRegex(self.source, r"\.checked\(\*id == active_channel\)")

    def test_channel_menu_click_writes_the_setting(self):
        """点击频道项必须落盘,否则重启后仍是旧频道。"""
        self.assertRegex(
            self.source,
            r'id\.starts_with\("channel-"\)\s*=>',
            "菜单事件里应处理 channel-* 的 id",
        )
        self.assertRegex(
            self.source,
            r'write_setting_value\(\s*"app-dsh-channel"\s*,\s*channel\s*\)',
        )

    def test_launcher_log_is_appended_so_failures_survive(self):
        """日志被 truncate 会抹掉上一次启动失败的现场 —— 那正是排查卡顿的唯一证据。"""
        spawn = self.source[
            self.source.index("fn spawn_dsh("):self.source.index("// pnpm dlx 会直接安装临时包")
        ]
        self.assertIn(".append(true)", spawn, "启动日志必须追加写入")
        self.assertNotIn(".truncate(true)", spawn, "启动日志不能用 truncate")

    def test_menu_exposes_all_plugin_update(self):
        self.assertIn('with_id("update-plugins", "一键更新全部插件")', self.source)
        self.assertIn('"version",', self.source)
        self.assertIn('env!("CARGO_PKG_VERSION")', self.source)
        self.assertRegex(
            self.source,
            r'"update-plugins"\s*=>\s*update_web_profile_plugins\s*\(\s*app\s*\)',
        )

    def test_plugin_update_is_non_interactive_and_updates_the_profile(self):
        self.assertIn("pnpm update", self.source)
        self.assertIn("npx -y pnpm update", self.source)
        self.assertRegex(
            self.source,
            re.compile(
                r'COREPACK_ENABLE_DOWNLOAD_PROMPT.*?npm_config_yes.*?NO_UPDATE_NOTIFIER',
                re.DOTALL,
            ),
        )
        self.assertIn('"正在更新 web profile 的全部插件...', self.source)
        self.assertIn("dsh-app-status-pulse", self.source)

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

    def test_progress_is_reported_before_and_during_the_backend_spawn(self):
        """版本解析可能有 5s 的 registry 请求;spawn 前就要让加载页显示阶段,
        否则那段时间窗口里没有任何反馈。"""
        main = self.source[
            self.source.index("fn main() {"):self.source.index("#[cfg(test)]")
        ]
        spawn = main.index("spawn_dsh(&spec)")
        resolve = main.index("let spec = resolve_dsh_spec();")
        self.assertRegex(
            main[:resolve],
            r"push_boot_progress\s*\(\s*&main_window\s*,\s*BootStage::Resolve",
            "解析版本前就要推送阶段",
        )
        self.assertRegex(
            main[resolve:spawn],
            r"push_boot_progress\s*\(\s*&main_window\s*,\s*BootStage::Spawn",
            "spawn 前要推送阶段并把 spec 作为细节显示",
        )

    def test_startup_poll_loop_reports_progress_and_failures(self):
        """启动轮询必须三件事齐全:推阶段、查后端是否提前退出、超时报错。"""
        window = self.source[
            self.source.index("let window = main_window.clone();"):
            self.source.index(".on_menu_event", self.source.index("let window = main_window.clone();"))
        ]
        self.assertRegex(window, r"push_boot_progress\s*\(\s*&window\s*,")
        self.assertRegex(window, r"push_boot_failure\s*\(\s*&window\s*,")
        self.assertIn("child.try_wait()", window, "必须检测后端是否已退出")
        self.assertIn("tail_launcher_log", window, "错误页要带日志现场")
        self.assertIn("timeout_secs", window, "错误信息要带实际等待时长")

    def test_synchronous_startup_loop_uses_blocking_worker(self):
        """reqwest blocking + sleep 不能放在永不 await 的 async task 中。"""
        window = self.source[
            self.source.index("let window = main_window.clone();"):
            self.source.index(".on_menu_event", self.source.index("let window = main_window.clone();"))
        ]
        self.assertIn("spawn_boot_worker(move || {", window)
        self.assertNotIn("async move", window)
        worker = re.search(
            r"fn spawn_boot_worker\b[\s\S]*?\n\}", self.source
        )
        self.assertIsNotNone(worker)
        self.assertIn("tauri::async_runtime::spawn_blocking(task)", worker.group())

    def test_failed_startup_releases_the_owned_backend(self):
        """自己拉起的后端起不来时不能把进程留着继续占 3080 端口。"""
        window = self.source[
            self.source.index("let window = main_window.clone();"):
            self.source.index(".on_menu_event", self.source.index("let window = main_window.clone();"))
        ]
        failure = window[window.index("push_boot_failure(&window, stage, &report);"):]
        self.assertRegex(
            failure,
            r"take_owned_child\s*\(\s*&state\s*\)",
            "超时后应取回并终止自己拉起的后端",
        )
        self.assertRegex(failure, r"terminate_child_tree\s*\(")

    def test_loading_page_exposes_the_progress_protocol(self):
        """Rust 用 window.eval 调 __dshBoot;两边的阶段 id 必须一致。"""
        page = (SOURCE_PATH.parents[2] / "dist/index.html").read_text()
        self.assertIn("window.__dshBoot", page)
        self.assertIn("update:", page)
        self.assertIn("fail:", page)
        for stage in ("probe", "resolve", "spawn", "download", "auth", "ready"):
            with self.subTest(stage=stage):
                self.assertIn(
                    "'{}'".format(stage),
                    page,
                    "加载页缺少阶段 {} —— Rust 侧推送对应 id 时不会高亮任何步骤".format(stage),
                )

    def test_loading_page_renders_failure_guidance(self):
        """失败要抛出来并给建议,而不是只留一个转圈。"""
        page = (SOURCE_PATH.parents[2] / "dist/index.html").read_text()
        self.assertIn("errtitle", page)
        self.assertIn("errreason", page)
        self.assertIn("errsteps", page)
        self.assertIn("err.classList.add('show')", page)

    def test_loading_page_stops_the_spinner_on_failure(self):
        """失败后不能继续转圈:必须换掉 spinner 状态并停掉耗时计时器。"""
        page = (SOURCE_PATH.parents[2] / "dist/index.html").read_text()
        self.assertIn("spinner failed", page)
        self.assertRegex(
            page,
            r"timerId\s*=\s*setInterval",
            "耗时计时器要存下 id,否则失败时无法停止",
        )
        self.assertRegex(page, r"clearInterval\s*\(\s*timerId\s*\)")

    def test_step_detail_is_written_after_the_steps_are_created(self):
        """步骤节点是首次 update 时懒创建的;在那之前写 detail 会落在不存在的节点上,
        表现为「阶段高亮了但后面的细节永远是空的」。"""
        page = (SOURCE_PATH.parents[2] / "dist/index.html").read_text()
        update_body = page[page.index("update: function"):page.index("fail: function")]
        paint = update_body.index("paintSteps();")
        detail = update_body.index("setDetail(payload.stage, payload.detail)")
        self.assertLess(paint, detail, "setDetail 必须排在 paintSteps 之后")

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

        spawn_calls = list(re.finditer(r"\bspawn_dsh\s*\(\s*&spec\s*\)", main))
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


class ShellLaunchScriptTests(unittest.TestCase):
    """真跑一遍从 main.rs 抽出的 shell 脚本,验证选路行为而不只是源码文本。

    脚本直接从源码抽取,测试不会与实现漂移。用 stub 顶替 pnpm/npx,
    所以既不联网也不会真装 220 MB 的包。
    """

    STUB = '#!/bin/sh\necho "STUB-%s $@"\nexit 0\n'
    SPEC = "@deepseek-ai/dsh@latest"

    @classmethod
    def setUpClass(cls):
        if sys.platform == "win32":
            raise unittest.SkipTest("Windows 走 cmd 分支,不是这里的 shell 脚本")
        source = SOURCE_PATH.read_text()
        marker = (
            '#[cfg(target_os = "macos")]'
            if sys.platform == "darwin"
            else '#[cfg(not(any(target_os = "windows", target_os = "macos")))]'
        )
        branch = source[source.index(marker):]
        cls.script = re.search(r'r#"(.*?)"#', branch, re.DOTALL).group(1).replace(
            "{spec}", cls.SPEC
        )
        cls.shell = (
            "/bin/zsh"
            if sys.platform == "darwin" and Path("/bin/zsh").exists()
            else "/bin/sh"
        )

    def run_script(self, *available):
        """在只有 `available` 这些命令的 PATH 下跑脚本,返回合并输出。"""
        with tempfile.TemporaryDirectory() as tmp:
            binaries, home = Path(tmp) / "bin", Path(tmp) / "home"
            binaries.mkdir()
            home.mkdir()
            for name in available:
                stub = binaries / name
                stub.write_text(self.STUB % name.upper())
                stub.chmod(0o755)
            done = subprocess.run(
                [self.shell, "-c", self.script],
                capture_output=True,
                text=True,
                # HOME 指向空目录:让脚本兜底探测的那些 pnpm 安装路径全部落空
                env={"PATH": f"{binaries}:/usr/bin:/bin", "HOME": str(home)},
                stdin=subprocess.DEVNULL,
                timeout=30,
            )
            return (done.stdout + done.stderr).strip(), done.returncode

    def test_pnpm_is_preferred_when_present(self):
        output, code = self.run_script("pnpm", "npx")
        self.assertEqual(output, f"STUB-PNPM dlx {self.SPEC} web --no-open")
        self.assertEqual(code, 0)

    def test_falls_back_to_non_interactive_npx_without_pnpm(self):
        """缺 pnpm 时必须回退 npx -y,而不是 exit 127 让窗口白等到超时。"""
        output, code = self.run_script("npx")
        self.assertEqual(output, f"STUB-NPX -y {self.SPEC} web --no-open")
        self.assertEqual(code, 0)

    def test_reports_a_clear_error_when_no_runner_exists(self):
        output, code = self.run_script()
        self.assertEqual(code, 127)
        self.assertIn("neither pnpm nor npx found", output)


if __name__ == "__main__":
    unittest.main()
