//! Secret broker and redaction pipeline: handles cross boundaries, values do
//! not, and a sink fails closed rather than emitting a credential.

use arsy_kernel::{
    domain::RequestId,
    protocol::ServerEvent,
    provider::anthropic::ApiKey,
    secret::{CredentialStore, SecretBroker, SecretError, SecretHandle},
    transport::StdioTransport,
};
use std::io::Cursor;

const API_KEY: &str = "sk-ant-live-0123456789";

/// Stand-in for the OS keyring: answers for its own store id and nothing else.
struct FakeKeyring {
    id: String,
    entries: Vec<(String, String)>,
}

impl CredentialStore for FakeKeyring {
    fn id(&self) -> &str {
        &self.id
    }

    fn resolve(&self, name: &str) -> Result<String, SecretError> {
        self.entries
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| SecretError::NotFound(SecretHandle::new(&self.id, name).unwrap()))
    }
}

fn keyring() -> Box<dyn CredentialStore> {
    Box::new(FakeKeyring {
        id: "keyring".to_owned(),
        entries: vec![("anthropic".to_owned(), API_KEY.to_owned())],
    })
}

fn handle() -> SecretHandle {
    SecretHandle::new("keyring", "anthropic").unwrap()
}

#[test]
fn a_handle_serializes_without_the_credential_and_round_trips() {
    let handle = handle();
    let encoded = serde_json::to_string(&handle).unwrap();

    assert_eq!(encoded, "\"secret://keyring/anthropic\"");
    assert!(!encoded.contains(API_KEY));
    assert_eq!(
        serde_json::from_str::<SecretHandle>(&encoded).unwrap(),
        handle
    );
    assert_eq!(
        serde_json::from_str::<SecretHandle>("\"keyring/anthropic\"")
            .unwrap_err()
            .to_string(),
        "malformed secret handle: keyring/anthropic"
    );
}

#[test]
fn resolution_needs_a_registered_store_and_never_falls_back() {
    let mut broker = SecretBroker::new();

    assert_eq!(
        broker.resolve(&handle()).unwrap_err(),
        SecretError::UnknownStore("keyring".to_owned()),
        "an unregistered store is an error, not a reason to read plaintext"
    );

    broker.register_store(keyring());
    let secret = broker.resolve(&handle()).unwrap();
    assert_eq!(secret.expose(), API_KEY);
    assert_eq!(
        format!("{secret:?}"),
        "SecretValue(secret://keyring/anthropic)"
    );
    assert_eq!(
        format!("{:?}", ApiKey::from_secret(&secret)),
        "ApiKey(redacted)"
    );

    let missing = SecretHandle::new("keyring", "openai").unwrap();
    assert_eq!(
        broker.resolve(&missing).unwrap_err(),
        SecretError::NotFound(missing)
    );
}

#[test]
fn redaction_is_idempotent_and_refuses_values_it_cannot_hide() {
    let mut broker = SecretBroker::new();
    broker.register_store(keyring());
    broker.resolve(&handle()).unwrap();
    let redactor = broker.redactor();

    let once = redactor
        .sanitize(&format!("authorization: {API_KEY} (retry with {API_KEY})"))
        .unwrap();
    assert_eq!(
        once,
        "authorization: [redacted:secret://keyring/anthropic] \
         (retry with [redacted:secret://keyring/anthropic])"
    );
    assert_eq!(
        redactor.sanitize(&once).unwrap(),
        once,
        "sanitizing an already sanitized payload changes nothing"
    );

    let short = SecretHandle::new("keyring", "pin").unwrap();
    let mut redactor = redactor.clone();
    assert_eq!(
        redactor.register(&short, "1234"),
        Err(SecretError::Unredactable(short)),
        "a value too short to match only itself is refused instead of half-redacted"
    );
}

#[test]
fn the_protocol_adapter_redacts_before_writing() {
    let mut broker = SecretBroker::new();
    broker.register_store(keyring());
    broker.resolve(&handle()).unwrap();

    let mut written = Vec::new();
    let mut transport = StdioTransport::with_redactor(
        Cursor::new(Vec::new()),
        &mut written,
        broker.redactor().clone(),
    );
    transport
        .write_event(ServerEvent::Failed {
            request_id: RequestId::new(),
            code: "provider_auth".to_owned(),
            message: format!("rejected key {API_KEY}"),
        })
        .unwrap();

    let line = String::from_utf8(written).unwrap();
    assert!(
        !line.contains(API_KEY),
        "credential reached the wire: {line}"
    );
    assert!(line.contains("[redacted:secret://keyring/anthropic]"));
}

/// A credential the operator keeps in a file, so the OS keychain is one option
/// rather than the only one. The permission check is the point: a key every
/// account on the machine can read is not a key.
#[test]
fn a_file_credential_resolves_only_when_its_owner_alone_can_read_it() {
    use arsy_kernel::secret::FileCredentialStore;

    let root = std::env::temp_dir().join(format!("arsy-file-cred-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("myai.key");
    let name = path.display().to_string();

    // Missing is not found, so resolution can fall through to another source
    // rather than failing the run outright.
    assert!(matches!(
        FileCredentialStore.resolve(&name),
        Err(SecretError::NotFound(_))
    ));

    // A shell redirect leaves the newline it added; it is not the credential.
    std::fs::write(&path, "sk-from-a-file\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = FileCredentialStore.resolve(&name).unwrap_err();
        assert!(
            format!("{error}").contains("chmod 600"),
            "a world-readable key was accepted: {error}"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    assert_eq!(
        FileCredentialStore.resolve(&name).unwrap(),
        "sk-from-a-file"
    );
    assert_eq!(FileCredentialStore.id(), "file");

    // An empty file is nothing to send, so it reads as absent rather than as a
    // credential that authenticates as nobody.
    std::fs::write(&path, "\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(matches!(
        FileCredentialStore.resolve(&name),
        Err(SecretError::NotFound(_))
    ));

    // The handle names the store it came from, so a redaction placeholder and
    // a diagnostic both say `file`.
    let handle = SecretHandle::try_from(format!("secret://file/{name}")).unwrap();
    assert_eq!(handle.store(), "file");

    // What `auth set` writes, `auth remove` has to be able to delete, or a
    // catalog that lists a file handle can only ever grow.
    std::fs::write(&path, "sk-from-a-file\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    FileCredentialStore.remove(&name).unwrap();
    assert!(!path.exists(), "the credential file survived removal");
    assert!(
        matches!(
            FileCredentialStore.remove(&name),
            Err(SecretError::NotFound(_))
        ),
        "removing what is already gone is not found, not a failure"
    );

    // FileCredentialStore::set writes owner-only file and makes value resolvable.
    FileCredentialStore
        .set(&name, "sk-written-by-set\n")
        .unwrap();
    assert_eq!(
        FileCredentialStore.resolve(&name).unwrap(),
        "sk-written-by-set"
    );
    FileCredentialStore.remove(&name).unwrap();

    std::fs::remove_dir_all(&root).unwrap();
}

/// `file` is the only store now, so the guards around it carry the whole
/// weight: a name may not walk out of the directory it resolves in, whichever
/// verb carries it, and a write may not leave a secret in a file the rest of
/// the machine can read.
#[test]
fn a_file_credential_is_guarded_the_same_way_whatever_the_verb() {
    use arsy_kernel::secret::FileCredentialStore;

    // Set already refused a traversing name. Remove and resolve are the same
    // question: a name that must not be written must not delete or read a file
    // outside either.
    for name in ["../escape.key", "nested/escape.key"] {
        for error in [
            FileCredentialStore.set(name, "x").unwrap_err(),
            FileCredentialStore.remove(name).unwrap_err(),
            FileCredentialStore.resolve(name).unwrap_err(),
        ] {
            assert!(
                format!("{error}").contains("traverse"),
                "`{name}` was not refused: {error}"
            );
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Writing over a file that already exists: `OpenOptions::mode` only
        // decides the mode of a file the call creates, so without a second
        // check the secret lands in whatever mode was there before.
        let root = std::env::temp_dir().join(format!("arsy-file-cred-mode-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reused.key");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let name = path.display().to_string();
        FileCredentialStore.set(&name, "sk-rewritten").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the rewritten credential is still world-readable"
        );
        assert_eq!(FileCredentialStore.resolve(&name).unwrap(), "sk-rewritten");

        // A reader racing a rewrite sees one whole credential or the other,
        // never a truncated file, and the write leaves nothing beside it.
        let long = "x".repeat(64 * 1024);
        let reader = {
            let name = name.clone();
            let long = long.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let read = FileCredentialStore.resolve(&name).unwrap();
                    assert!(read == "sk-rewritten" || read == long, "a torn read");
                }
            })
        };
        for round in 0..200 {
            let value = if round % 2 == 0 {
                long.as_str()
            } else {
                "sk-rewritten"
            };
            FileCredentialStore.set(&name, value).unwrap();
        }
        reader.join().unwrap();
        let entries = std::fs::read_dir(&root).unwrap().count();
        assert_eq!(entries, 1, "a staging file was left behind");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
