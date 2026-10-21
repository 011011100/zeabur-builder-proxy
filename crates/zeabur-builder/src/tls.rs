//! mTLS between build Jobs and buildkitd (plan §7.1): the controller makes a CA, a server
//! certificate for every buildkitd Pod and one client certificate, on first start.
//!
//! Each Secret holds `ca.pem`, `cert.pem` and `key.pem`, the names `buildctl --tlsdir` and
//! buildkitd's `--tlscacert`/`--tlscert`/`--tlskey` read. The server certificate names
//! `*.<service>.<namespace>.svc` and its `cluster.local` form, so it covers every `StatefulSet` Pod
//! behind the headless Service. The CA key is not kept: nothing is signed after the first start.
//! Rotation is not implemented; the certificates are valid until rcgen's default end date.

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, ObjectMeta, PostParams};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use zeabur_builder_api::labels;

/// The PEM files of one side.
pub(crate) type Pems = BTreeMap<String, String>;

/// Generates the CA, the server certificate and the client certificate.
pub(crate) fn generate(service: &str, namespace: &str) -> Result<(Pems, Pems), rcgen::Error> {
    let ca_key = KeyPair::generate()?;
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name
        .push(DnType::CommonName, "zeabur-builder buildkitd CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_pem = ca.self_signed(&ca_key)?.pem();
    let issuer = Issuer::new(ca, ca_key);

    let leaf = |names: Vec<String>, cn: &str, usage: ExtendedKeyUsagePurpose| {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(names)?;
        params.distinguished_name.push(DnType::CommonName, cn);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![usage];
        let cert = params.signed_by(&key, &issuer)?;
        Ok::<_, rcgen::Error>(Pems::from([
            ("ca.pem".to_owned(), ca_pem.clone()),
            ("cert.pem".to_owned(), cert.pem()),
            ("key.pem".to_owned(), key.serialize_pem()),
        ]))
    };
    let server = leaf(
        vec![
            format!("*.{service}.{namespace}.svc"),
            format!("*.{service}.{namespace}.svc.cluster.local"),
        ],
        service,
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    let client = leaf(
        Vec::new(),
        "zeabur-builder-job",
        ExtendedKeyUsagePurpose::ClientAuth,
    )?;
    Ok((server, client))
}

fn secret(name: &str, namespace: &str, pems: &Pems) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(BTreeMap::from([(
                labels::MANAGED_BY.to_owned(),
                labels::MANAGER.to_owned(),
            )])),
            ..ObjectMeta::default()
        },
        data: Some(
            pems.iter()
                .map(|(k, v)| (k.clone(), ByteString(v.clone().into_bytes())))
                .collect(),
        ),
        ..Secret::default()
    }
}

/// Where the certificates live.
#[derive(Debug, Clone)]
pub(crate) struct Locations {
    /// The headless Service of the buildkitd `StatefulSet`.
    pub(crate) service: String,
    /// buildkitd's namespace, where the server Secret goes.
    pub(crate) buildkitd_namespace: String,
    /// The server Secret, mounted by buildkitd.
    pub(crate) server_secret: String,
    /// The controller's namespace, where the client Secret is kept.
    pub(crate) controller_namespace: String,
    /// The client Secret, copied into every namespace that runs builds.
    pub(crate) client_secret: String,
}

/// Creates the certificates unless both Secrets exist.
pub(crate) async fn ensure(client: &Client, at: &Locations) -> Result<(), kube::Error> {
    let servers: Api<Secret> = Api::namespaced(client.clone(), &at.buildkitd_namespace);
    let clients: Api<Secret> = Api::namespaced(client.clone(), &at.controller_namespace);
    if servers.get_opt(&at.server_secret).await?.is_some()
        && clients.get_opt(&at.client_secret).await?.is_some()
    {
        return Ok(());
    }
    let (server, client_pems) = generate(&at.service, &at.buildkitd_namespace)
        .map_err(|e| kube::Error::Service(e.to_string().into()))?;
    for (api, name, namespace, pems) in [
        (
            &servers,
            &at.server_secret,
            &at.buildkitd_namespace,
            &server,
        ),
        (
            &clients,
            &at.client_secret,
            &at.controller_namespace,
            &client_pems,
        ),
    ] {
        let wanted = secret(name, namespace, pems);
        if api.get_opt(name).await?.is_some() {
            api.replace(name, &PostParams::default(), &wanted).await?;
        } else {
            api.create(&PostParams::default(), &wanted).await?;
        }
    }
    tracing::info!(secret = %at.server_secret, "created buildkitd certificates");
    Ok(())
}

/// Copies the controller's Secret `name` into `namespace` under `target`, when missing or
/// different.
pub(crate) async fn copy_secret(
    client: &Client,
    from_namespace: &str,
    name: &str,
    namespace: &str,
    target: &str,
) -> Result<(), kube::Error> {
    let source = Api::<Secret>::namespaced(client.clone(), from_namespace)
        .get(name)
        .await?;
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let mut wanted = Secret {
        metadata: ObjectMeta {
            name: Some(target.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(BTreeMap::from([(
                labels::MANAGED_BY.to_owned(),
                labels::MANAGER.to_owned(),
            )])),
            ..ObjectMeta::default()
        },
        data: source.data,
        type_: source.type_,
        ..Secret::default()
    };
    match api.get_opt(target).await? {
        Some(existing) if existing.data == wanted.data => Ok(()),
        Some(existing) => {
            wanted.metadata.resource_version = existing.metadata.resource_version;
            api.replace(target, &PostParams::default(), &wanted)
                .await
                .map(drop)
        }
        // Runs started together race to create the copy; the loser finds it there.
        None => match api.create(&PostParams::default(), &wanted).await {
            Err(kube::Error::Api(status)) if status.is_already_exists() => Ok(()),
            result => result.map(drop),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_certificate_covers_every_pod() {
        let (server, client) = generate("buildkitd", "builder-system").unwrap();
        assert_eq!(server["ca.pem"], client["ca.pem"]);
        for pems in [&server, &client] {
            assert!(pems["cert.pem"].starts_with("-----BEGIN CERTIFICATE-----"));
            assert!(pems["key.pem"].starts_with("-----BEGIN PRIVATE KEY-----"));
        }
        let der = der_of(&server["cert.pem"]);
        let text = String::from_utf8_lossy(&der);
        assert!(text.contains("*.buildkitd.builder-system.svc.cluster.local"));
    }

    /// The DER of the first PEM block, enough to search for a SAN.
    fn der_of(pem: &str) -> Vec<u8> {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject as _;
        CertificateDer::from_pem_slice(pem.as_bytes())
            .unwrap()
            .to_vec()
    }
}
