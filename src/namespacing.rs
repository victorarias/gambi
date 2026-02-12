use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

pub fn namespaced(server_name: &str, capability_name: &str) -> String {
    format!("{server_name}:{capability_name}")
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    let (server, capability) = name.split_once(':')?;
    if server.is_empty() || capability.is_empty() {
        return None;
    }
    Some((server, capability))
}

pub fn namespaced_resource_uri(server_name: &str, upstream_uri: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(upstream_uri.as_bytes());
    format!("gambi://resource/{server_name}/{encoded}")
}

pub fn parse_namespaced_resource_uri(uri: &str) -> Option<(String, String)> {
    let prefix = "gambi://resource/";
    let rest = uri.strip_prefix(prefix)?;
    let (server, encoded) = rest.split_once('/')?;
    if server.is_empty() || encoded.is_empty() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let upstream_uri = String::from_utf8(decoded).ok()?;
    Some((server.to_string(), upstream_uri))
}

pub fn namespaced_resource_template_uri(server_name: &str, upstream_uri_template: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(upstream_uri_template.as_bytes());
    format!("gambi://template/{server_name}/{encoded}")
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_namespaced_resource_template_uri(uri_template: &str) -> Option<(String, String)> {
    let prefix = "gambi://template/";
    let rest = uri_template.strip_prefix(prefix)?;
    let (server, encoded) = rest.split_once('/')?;
    if server.is_empty() || encoded.is_empty() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let upstream_uri_template = String::from_utf8(decoded).ok()?;
    Some((server.to_string(), upstream_uri_template))
}

#[cfg(test)]
mod tests {
    use super::{
        namespaced, namespaced_resource_template_uri, namespaced_resource_uri,
        parse_namespaced_resource_template_uri, parse_namespaced_resource_uri, split_namespaced,
    };
    use proptest::prelude::*;

    #[test]
    fn creates_namespaced_name() {
        assert_eq!(namespaced("port", "list_entities"), "port:list_entities");
    }

    #[test]
    fn splits_namespaced_name() {
        assert_eq!(
            split_namespaced("atlassian:search_issues"),
            Some(("atlassian", "search_issues"))
        );
    }

    #[test]
    fn rejects_invalid_namespaced_name() {
        assert!(split_namespaced(":missing_server").is_none());
        assert!(split_namespaced("missing_capability:").is_none());
        assert!(split_namespaced("no_separator").is_none());
    }

    #[test]
    fn resource_uri_roundtrip() {
        let upstream = "file:///tmp/test.txt";
        let namespaced = namespaced_resource_uri("port", upstream);
        let parsed = parse_namespaced_resource_uri(&namespaced).expect("must parse");
        assert_eq!(parsed.0, "port");
        assert_eq!(parsed.1, upstream);
    }

    #[test]
    fn resource_template_roundtrip() {
        let upstream = "file:///tmp/{path}";
        let namespaced = namespaced_resource_template_uri("port", upstream);
        let parsed =
            parse_namespaced_resource_template_uri(&namespaced).expect("must parse template");
        assert_eq!(parsed.0, "port");
        assert_eq!(parsed.1, upstream);
    }

    proptest! {
        #[test]
        fn split_namespaced_roundtrip_property(
            server in "[A-Za-z0-9_-]{1,24}",
            capability in "[A-Za-z0-9_-]{1,40}",
        ) {
            let full = namespaced(&server, &capability);
            let parsed = split_namespaced(&full).expect("must parse generated namespaced string");
            prop_assert_eq!(parsed.0, server);
            prop_assert_eq!(parsed.1, capability);
        }

        #[test]
        fn resource_uri_roundtrip_property(
            server in "[A-Za-z0-9_-]{1,24}",
            upstream in "[A-Za-z0-9:/?&=_.\\-]{1,120}",
        ) {
            let wrapped = namespaced_resource_uri(&server, &upstream);
            let parsed = parse_namespaced_resource_uri(&wrapped).expect("must parse generated resource uri");
            prop_assert_eq!(parsed.0, server);
            prop_assert_eq!(parsed.1, upstream);
        }

        #[test]
        fn template_uri_roundtrip_property(
            server in "[A-Za-z0-9_-]{1,24}",
            upstream in "[A-Za-z0-9:/?&=_.\\-\\{\\}]{1,120}",
        ) {
            let wrapped = namespaced_resource_template_uri(&server, &upstream);
            let parsed = parse_namespaced_resource_template_uri(&wrapped).expect("must parse generated template uri");
            prop_assert_eq!(parsed.0, server);
            prop_assert_eq!(parsed.1, upstream);
        }
    }
}
