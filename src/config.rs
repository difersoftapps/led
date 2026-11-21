//! Leitura/gravacao do `led.cfg`, no mesmo formato "shell" usado pelo `ledctl.sh`
//! (`ENABLED=1`, `MODE=3`, `BRI=5`, `LEDCOUNT=12`), para que o menu antigo, o
//! `ledctl.sh apply` do boot e este app enxerguem sempre a mesma configuracao.

use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

use crate::driver::{BRI_MAX, BRI_MIN};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub mode: u8,
    pub bri: u8,
    pub ledcount: u32,
}

impl Default for Config {
    /// Mesmos padroes do ledctl.sh: apagado, vermelho fixo, brilho medio, 12 LEDs.
    fn default() -> Self {
        Config {
            enabled: false,
            mode: 3,
            bri: 5,
            ledcount: 12,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches(|ch| ch == '"' || ch == '\'');
            match k.trim() {
                "ENABLED" => c.enabled = v == "1",
                "MODE" => {
                    if let Ok(n) = v.parse() {
                        c.mode = n;
                    }
                }
                "BRI" => {
                    if let Ok(n) = v.parse::<u8>() {
                        c.bri = n.clamp(BRI_MIN, BRI_MAX);
                    }
                }
                "LEDCOUNT" => {
                    if let Ok(n) = v.parse() {
                        c.ledcount = n;
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        format!(
            "ENABLED={}\nMODE={}\nBRI={}\nLEDCOUNT={}\n",
            self.enabled as u8, self.mode, self.bri, self.ledcount
        )
    }

    /// Arquivo ausente ou ilegivel => padroes.
    pub fn load(path: &Path) -> Config {
        fs::read_to_string(path)
            .map(|t| Config::parse(&t))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.to_text())?;
        // Como o `chown ark:ark` do script: quando rodando como root, devolve o
        // arquivo ao dono da pasta para o usuario `ark` conseguir editar depois.
        if let Some(dir) = path.parent() {
            if let Ok(m) = fs::metadata(dir) {
                let _ = std::os::unix::fs::chown(path, Some(m.uid()), Some(m.gid()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = Config {
            enabled: true,
            mode: 19,
            bri: 7,
            ledcount: 12,
        };
        assert_eq!(Config::parse(&c.to_text()), c);
        assert_eq!(c.to_text(), "ENABLED=1\nMODE=19\nBRI=7\nLEDCOUNT=12\n");
    }

    #[test]
    fn parse_tolerante() {
        let c = Config::parse("# x\nENABLED=\"1\"\nMODE=abc\nBRI=99\nlixo\nLEDCOUNT=8\nOUTRA=1\n");
        assert!(c.enabled);
        assert_eq!(c.mode, 3); // invalido -> mantem o padrao
        assert_eq!(c.bri, BRI_MAX); // limitado
        assert_eq!(c.ledcount, 8);
    }

    #[test]
    fn arquivo_inexistente_usa_padrao() {
        assert_eq!(
            Config::load(Path::new("/nao/existe/led.cfg")),
            Config::default()
        );
    }
}
