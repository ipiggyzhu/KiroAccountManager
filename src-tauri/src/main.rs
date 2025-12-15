#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod auth_social;
mod aws_sso_client;
mod browser;
mod codewhisperer_client;
mod commands;
mod deep_link_handler;

mod kiro;
mod kiro_auth_client;
mod mcp;
mod powers;
mod process;
mod providers;
mod state;
mod steering;
mod account;

use account::AccountStore;
use auth::AuthState;
use state::AppState;
use std::sync::Mutex;
use tauri::{Emitter, Listener, Manager};

// 导入命令
use browser::detect_installed_browsers;
use commands::account_cmd::{
    get_accounts, delete_account, delete_accounts, update_account, sync_account,
    refresh_account_token, verify_account, add_account_by_social, add_local_kiro_account,
    add_account_by_idc, import_accounts, export_accounts
};
use commands::app_settings_cmd::*;
use commands::auth_cmd::*;
use commands::kiro_settings_cmd::*;
use commands::machine_guid_cmd::*;
use commands::mcp_cmd::*;
use commands::powers_cmd::*;
use commands::proxy_cmd::*;
use commands::sso_import_cmd::*;
use commands::update_cmd::*;
use commands::web_oauth_cmd::*;
use commands::steering_cmd::*;
use kiro::{
    get_kiro_local_token, get_kiro_telemetry_info, reset_kiro_machine_id, switch_kiro_account,
};
use process::{close_kiro_ide, is_kiro_ide_running, start_kiro_ide};

/// Windows: 修复注册表中的 deep link 格式
/// Tauri 自动注册的格式缺少 --open-url 和 -- 参数
#[cfg(windows)]
fn fix_deep_link_registry() -> Result<(), Box<dyn std::error::Error>> {
    use winreg::enums::*;
    use winreg::RegKey;
    
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = r"Software\Classes\kiro\shell\open\command";
    
    let (key, _) = hkcu.create_subkey(path)?;
    
    // 获取当前可执行文件路径
    let exe_path = std::env::current_exe()?;
    let exe_str = exe_path.to_string_lossy();
    
    // 设置正确的格式：包含 --open-url 和 -- 参数
    let value = format!("\"{}\" --open-url -- \"%1\"", exe_str);
    key.set_value("", &value)?;
    
    println!("[Registry] Updated deep link command: {}", value);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_deep_link::init())
        // Single-instance 插件：处理 Windows 新实例启动时的 URL 传递
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            println!("\n========== SINGLE INSTANCE CALLBACK ==========");
            println!("[SingleInstance] Args count: {}", args.len());
            for (i, arg) in args.iter().enumerate() {
                println!("[SingleInstance] args[{}] = {}", i, arg);
            }
            
            // Windows 点击 kiro:// 链接时启动新实例
            // 参数格式可能是:
            // - [exe_path, --open-url, --, url] (4个参数，URL在args[3])
            // - [exe_path, url] (2个参数，URL在args[1])
            // - [exe_path, --open-url, url] (3个参数，URL在args[2])
            let url = if args.len() > 3 {
                Some(&args[3])
            } else if args.len() > 1 && args[1].starts_with("kiro://") {
                Some(&args[1])
            } else if args.len() > 2 && args[2].starts_with("kiro://") {
                Some(&args[2])
            } else {
                None
            };
            
            if let Some(url) = url {
                println!("[SingleInstance] ✓ Found URL: {}", url);
                println!("[SingleInstance] Emitting deep-link://new-url event...");
                match app.emit("deep-link://new-url", url.as_str()) {
                    Ok(_) => println!("[SingleInstance] ✓ Event emitted successfully"),
                    Err(e) => println!("[SingleInstance] ✗ Event emit failed: {}", e),
                }
            } else {
                println!("[SingleInstance] ✗ No kiro:// URL found in args");
            }
            
            // 聚焦主窗口
            if let Some(window) = app.get_webview_window("main") {
                println!("[SingleInstance] Focusing main window...");
                let _ = window.show();
                let _ = window.set_focus();
            }
            println!("========== END SINGLE INSTANCE ==========\n");
        }))
        .setup(|app| {
            // 注册 deep link 协议 (kiro://)
            #[cfg(any(target_os = "linux", windows))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let _ = app.deep_link().register("kiro");
                println!("[Setup] Deep link protocol 'kiro://' registered");
            }
            
            // Windows: 修复注册表格式（确保包含 --open-url 和 -- 参数）
            #[cfg(windows)]
            {
                match fix_deep_link_registry() {
                    Ok(_) => println!("[Setup] ✓ Registry updated successfully"),
                    Err(e) => println!("[Setup] ✗ Registry update failed: {}", e),
                }
            }
            
            // 监听 deep link URL 事件
            let app_handle = app.handle().clone();
            app.listen("deep-link://new-url", move |event| {
                println!("\n========== DEEP LINK EVENT RECEIVED ==========");
                let payload = event.payload();
                println!("[DeepLink] Raw payload: {}", payload);
                println!("[DeepLink] Payload length: {}", payload.len());
                
                // Tauri 事件 payload 是 JSON 格式，需要反序列化
                // payload 格式: "\"kiro://...\"" (包含转义引号)
                let url: String = match serde_json::from_str(payload) {
                    Ok(u) => {
                        println!("[DeepLink] ✓ JSON parsed URL: {}", u);
                        u
                    }
                    Err(e) => {
                        println!("[DeepLink] ✗ JSON parse failed: {}", e);
                        println!("[DeepLink] Using raw payload as URL");
                        payload.to_string()
                    }
                };
                
                // 处理 OAuth 回调
                println!("[DeepLink] Calling handle_deep_link with URL: {}", url);
                let handled = deep_link_handler::handle_deep_link(&url);
                println!("[DeepLink] Handle result: {}", if handled { "✓ SUCCESS" } else { "✗ FAILED" });
                
                // 聚焦窗口
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.set_focus();
                }
                println!("========== END DEEP LINK EVENT ==========\n");
            });
            
            Ok(())
        })
        .manage(AppState {
            store: Mutex::new(AccountStore::new()),
            auth: AuthState::new(),
            pending_login: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            // 账号命令
            get_accounts,
            delete_account,
            delete_accounts,
            update_account,
            sync_account,
            refresh_account_token,
            verify_account,
            add_account_by_social,
            add_local_kiro_account,
            add_account_by_idc,
            import_accounts,
            export_accounts,
            // Auth 命令
            get_current_user,
            logout,
            kiro_login,
            get_supported_providers,
            handle_kiro_social_callback,
            add_kiro_account,
            // Kiro IDE 命令
            get_kiro_local_token,
            switch_kiro_account,
            get_kiro_telemetry_info,
            reset_kiro_machine_id,
            // 进程管理命令
            close_kiro_ide,
            start_kiro_ide,
            is_kiro_ide_running,
            // Kiro IDE 设置命令
            get_kiro_settings,
            set_kiro_proxy,
            set_kiro_model,
            // 应用设置命令
            get_app_settings,
            save_app_settings,
            // 账号绑定机器码命令
            bind_machine_id_to_account,
            unbind_machine_id_from_account,
            get_bound_machine_id,
            get_all_bound_machine_ids,
            // 系统机器码命令
            get_system_machine_guid,
            backup_machine_guid,
            restore_machine_guid,
            reset_system_machine_guid,
            get_machine_guid_backup,
            set_custom_machine_guid,
            clear_macos_override,
            generate_machine_guid,
            // Web OAuth 命令 (Cognito + CBOR)
            web_oauth_initiate,
            web_oauth_complete,
            web_oauth_refresh,
            web_oauth_login,
            web_oauth_close_window,
            // 浏览器检测
            detect_installed_browsers,
            // MCP 管理命令
            get_mcp_config,
            save_mcp_server,
            delete_mcp_server,
            toggle_mcp_server,
            // Powers 管理命令
            get_powers_registry,
            get_installed_powers,
            get_all_powers,
            install_power,
            uninstall_power,
            // 代理检测命令
            detect_system_proxy,
            // SSO Token 导入命令
            import_from_sso_token,
            // 更新检查命令
            check_update,
            // Steering 管理命令
            get_steering_files,
            get_steering_file,
            save_steering_file,
            delete_steering_file,
            create_steering_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
