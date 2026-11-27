//! Lightweight XML parsers for AWS EC2 and STS Query API responses.

use open_ci_core::SecretString;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::BTreeMap;

/// Parsed temporary credentials from an AWS STS `AssumeRoleResponse`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedStsCredentials {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
    pub session_token: SecretString,
    pub expiration_rfc3339: String,
}

/// Parsed EC2 instance metadata from `RunInstances` or `DescribeInstances`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedEc2Instance {
    pub instance_id: String,
    pub state_name: String,
    pub availability_zone: String,
    pub launch_time_rfc3339: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// Parsed EBS volume metadata from `DescribeVolumes`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedEbsVolume {
    pub volume_id: String,
    pub status: String,
    pub attached: bool,
    pub create_time_rfc3339: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// Extracts `<Code>...</Code>` from an AWS XML error response.
pub fn parse_error_code(xml: &str) -> Option<String> {
    extract_first_tag(xml, "Code")
}

/// Extracts `<Account>...</Account>` from STS `GetCallerIdentity`.
pub fn parse_sts_account(xml: &str) -> Option<String> {
    extract_first_tag(xml, "Account")
}

/// Extracts `<nextToken>...</nextToken>` from paginated EC2 Query responses.
pub fn parse_next_token(xml: &str) -> Option<String> {
    extract_first_tag(xml, "nextToken").filter(|token| !token.is_empty())
}

/// Extracts `<Credentials>` from an STS `AssumeRoleResponse`.
pub fn parse_assume_role_credentials(xml: &str) -> Option<ParsedStsCredentials> {
    let mut reader = Reader::from_str(xml);
    let mut in_credentials = false;
    let mut current_tag = String::new();
    let mut access_key_id = String::new();
    let mut secret_access_key = String::new();
    let mut session_token = String::new();
    let mut expiration_rfc3339 = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "Credentials" {
                    in_credentials = true;
                } else if in_credentials {
                    current_tag = name;
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "Credentials" {
                    break;
                }
                if in_credentials && current_tag == name {
                    current_tag.clear();
                }
            }
            Ok(Event::Text(e)) if in_credentials => {
                if let Ok(text) = e.unescape() {
                    match current_tag.as_str() {
                        "AccessKeyId" => access_key_id.push_str(&text),
                        "SecretAccessKey" => secret_access_key.push_str(&text),
                        "SessionToken" => session_token.push_str(&text),
                        "Expiration" => expiration_rfc3339.push_str(&text),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if access_key_id.is_empty()
        || secret_access_key.is_empty()
        || session_token.is_empty()
        || expiration_rfc3339.is_empty()
    {
        return None;
    }

    Some(ParsedStsCredentials {
        access_key_id,
        secret_access_key: SecretString::new(secret_access_key),
        session_token: SecretString::new(session_token),
        expiration_rfc3339,
    })
}

/// Parses instances from `RunInstancesResponse` or `DescribeInstancesResponse`.
pub fn parse_instances(xml: &str) -> Vec<ParsedEc2Instance> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut instances = Vec::new();
    let mut current: Option<ParsedEc2Instance> = None;
    let mut tag_key = String::new();
    let mut tag_value = String::new();
    let mut in_tag = false;
    let mut in_instance_state = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "item" && stack.last().is_some_and(|p| p == "instancesSet") {
                    current = Some(ParsedEc2Instance::default());
                } else if name == "instanceState" && current.is_some() {
                    in_instance_state = true;
                } else if name == "item"
                    && current.is_some()
                    && stack.last().is_some_and(|p| p == "tagSet")
                {
                    in_tag = true;
                    tag_key.clear();
                    tag_value.clear();
                }
                stack.push(name);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                stack.pop();
                if name == "instanceState" {
                    in_instance_state = false;
                } else if name == "item" && in_tag {
                    if let Some(inst) = current.as_mut()
                        && !tag_key.is_empty()
                    {
                        inst.tags.insert(tag_key.clone(), tag_value.clone());
                    }
                    in_tag = false;
                } else if name == "item"
                    && stack.last().is_some_and(|p| p == "instancesSet")
                    && let Some(inst) = current.take()
                    && !inst.instance_id.is_empty()
                {
                    instances.push(inst);
                }
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .unescape()
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
                let current_tag = stack.last().map_or("", String::as_str);
                if in_tag {
                    if current_tag == "key" {
                        tag_key.push_str(&text);
                    } else if current_tag == "value" {
                        tag_value.push_str(&text);
                    }
                } else if let Some(inst) = current.as_mut() {
                    let parent = stack
                        .get(stack.len().saturating_sub(2))
                        .map_or("", String::as_str);
                    match current_tag {
                        "instanceId" if parent == "item" => inst.instance_id.push_str(&text),
                        "name" if in_instance_state => inst.state_name.push_str(&text),
                        "availabilityZone" => inst.availability_zone.push_str(&text),
                        "launchTime" => {
                            inst.launch_time_rfc3339
                                .get_or_insert_with(String::new)
                                .push_str(&text);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    instances
}

/// Parses volumes from `DescribeVolumesResponse`.
pub fn parse_volumes(xml: &str) -> Vec<ParsedEbsVolume> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut volumes = Vec::new();
    let mut current: Option<ParsedEbsVolume> = None;
    let mut tag_key = String::new();
    let mut tag_value = String::new();
    let mut in_tag = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "item" && stack.last().is_some_and(|p| p == "volumeSet") {
                    current = Some(ParsedEbsVolume::default());
                } else if name == "item"
                    && current.is_some()
                    && stack.last().is_some_and(|p| p == "attachmentSet")
                {
                    if let Some(vol) = current.as_mut() {
                        vol.attached = true;
                    }
                } else if name == "item"
                    && current.is_some()
                    && stack.last().is_some_and(|p| p == "tagSet")
                {
                    in_tag = true;
                    tag_key.clear();
                    tag_value.clear();
                }
                stack.push(name);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                stack.pop();
                if name == "item" && in_tag {
                    if let Some(vol) = current.as_mut()
                        && !tag_key.is_empty()
                    {
                        vol.tags.insert(tag_key.clone(), tag_value.clone());
                    }
                    in_tag = false;
                } else if name == "item"
                    && stack.last().is_some_and(|p| p == "volumeSet")
                    && let Some(vol) = current.take()
                    && !vol.volume_id.is_empty()
                {
                    volumes.push(vol);
                }
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .unescape()
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
                let current_tag = stack.last().map_or("", String::as_str);
                let parent = stack
                    .get(stack.len().saturating_sub(2))
                    .map_or("", String::as_str);
                if in_tag {
                    if current_tag == "key" {
                        tag_key.push_str(&text);
                    } else if current_tag == "value" {
                        tag_value.push_str(&text);
                    }
                } else if let Some(vol) = current.as_mut() {
                    match current_tag {
                        "volumeId" if parent == "item" => vol.volume_id.push_str(&text),
                        "status" if parent == "item" => vol.status.push_str(&text),
                        "createTime" => {
                            vol.create_time_rfc3339
                                .get_or_insert_with(String::new)
                                .push_str(&text);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    volumes
}

fn extract_first_tag(xml: &str, target: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut inside = false;
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == target.as_bytes() {
                    inside = true;
                    buf.clear();
                }
            }
            Ok(Event::Text(e)) if inside => {
                if let Ok(t) = e.unescape() {
                    buf.push_str(&t);
                }
            }
            Ok(Event::End(e)) => {
                if inside && e.local_name().as_ref() == target.as_bytes() {
                    return Some(buf);
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}
