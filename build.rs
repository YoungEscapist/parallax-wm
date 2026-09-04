//! Единственная задача: вписать в бинарь хеш коммита, из которого он собран.
//!
//! Зачем. Отчёт об ошибке начинается со строки `plx-extra --version`, и версии
//! `0.1.0` в нём мало: между тегами лежат сотни коммитов, а собирают люди из
//! master. Хеш отвечает на «а из чего это вообще собрано» одной строкой.
//!
//! Из архива без `.git` (или без установленного git) переменная не задаётся
//! вовсе — `option_env!("PLX_COMMIT")` в lib.rs это учитывает и печатает
//! `unknown commit`. Сборка при этом не ломается: причина, по которой здесь
//! нет ни одного `unwrap`.

use std::process::Command;

fn main() {
    // Пересобирать при переходе на другой коммит или другую ветку. Без этих
    // строк cargo не знал бы, что от .git вообще что-то зависит, и хеш
    // застревал бы на том, каким был при первой сборке.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let вывод = Command::new("git")
        .args(["describe", "--always", "--dirty=+", "--abbrev=7"])
        .output();

    if let Ok(вывод) = вывод {
        if вывод.status.success() {
            let хеш = String::from_utf8_lossy(&вывод.stdout).trim().to_string();
            if !хеш.is_empty() {
                println!("cargo:rustc-env=PLX_COMMIT={хеш}");
            }
        }
    }
}
