
    // ---- K8 根因探针：配置/会话路径测试隔离 ----
    // 历史坑（K8）：config crate 作依赖被 shell 测试编译时，其自身 `cfg!(test)` 不生效
    // （编译期仅对「被测试的 crate」置位）→ `UserPrefs::load()` 直读真实
    // `%APPDATA%\gpui-editor\config.json`，font_size / opened_folder / session_restore /
    // recent_* 泄漏进 shell 测试，成批预存失败。根因修复后：不设 `GPUI_EDITOR_CONFIG` 时
    // 配置路径必须落在隔离临时目录，load() 回退全默认。

    /// 探针：不设 `GPUI_EDITOR_CONFIG` 时，`config_path()`/`session_dir()` 落隔离临时目录
    /// 而非真实 `%APPDATA%\gpui-editor`；`load()` 回退全默认（真实配置字段不得泄漏）。
    /// 显式设置 `GPUI_EDITOR_CONFIG`（既有测试靠它定向隔离）仍优先，不受本探针影响。
    #[test]
    fn config_path_isolated_to_temp_dir_in_tests() {
        unsafe { std::env::remove_var("GPUI_EDITOR_CONFIG") };
        let cfg = config::user::UserPrefs::config_path();
        let sess = config::user::UserPrefs::session_dir();
        let real_root = std::env::var("APPDATA")
            .map(|a| std::path::PathBuf::from(a).join("gpui-editor"))
            .unwrap_or_default();
        assert_eq!(
            cfg.starts_with(&real_root),
            false,
            "config_path 不得触碰真实 %APPDATA%\\gpui-editor：{cfg:?}"
        );
        assert_eq!(
            sess.starts_with(&real_root),
            false,
            "session_dir 不得触碰真实 %APPDATA%\\gpui-editor：{sess:?}"
        );
        assert!(
            cfg.starts_with(std::env::temp_dir()),
            "config_path 应落在隔离临时目录（进程唯一）：{cfg:?}"
        );
        let prefs = config::user::UserPrefs::load();
        assert!(
            prefs.opened_folder.is_none(),
            "opened_folder 不得泄漏真实配置：{:?}",
            prefs.opened_folder
        );
        assert_eq!(prefs.font_size, None, "font_size 不得泄漏真实配置");
    }
