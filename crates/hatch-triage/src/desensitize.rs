//! Build the desensitize mapping for one task from the local database.
//!
//! The canonical set (see `AddressMapper::build`): the mailbox owner
//! plus every mail participant plus every contact. `mail_search` in
//! `hatch-tools` builds the same set inline — it cannot depend on this
//! crate — so any change to the set here must be mirrored there, or
//! the two mappers will assign different codes to the same address.

use hatch_core::desensitize::AddressMapper;
use hatch_core::Result;
use hatch_memory::People;
use hatch_store::Db;

/// The mapping for this task's mailbox: owner first, then every
/// distinct sender / recipient / contact, sorted by address.
pub fn mapper_for(db: &Db, owner_addr: &str, owner_name: &str) -> Result<AddressMapper> {
    let mut contacts: Vec<(String, String)> = Vec::new();
    for p in People::new(db).list()? {
        contacts.push((p.email, p.name));
    }
    for (addr, name) in hatch_connectors::MailRepo::new(db).participants()? {
        contacts.push((addr, name));
    }
    Ok(AddressMapper::build(owner_addr, owner_name, &contacts))
}
