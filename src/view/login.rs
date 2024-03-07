use std::{fs::File, io::Read, str};

use librespot_discovery::Credentials;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct LoginForm {
    username: String,
    password: String,
}

impl From<LoginForm> for Credentials {
    fn from(val: LoginForm) -> Self {
        Credentials::with_password(val.username, val.password)
    }
}

pub fn get_qml_view() -> Result<String, String> {
    let mut root_file = File::open("res/qml/main.qml").map_err(|e| e.to_string())?;
    let mut buf: Vec<u8> = Vec::new();
    root_file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(str::from_utf8(&buf).map_err(|e| e.to_string())?.to_owned())
}
