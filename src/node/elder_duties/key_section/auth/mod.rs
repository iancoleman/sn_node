// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod auth_keys;

pub use self::auth_keys::AuthKeysDb;
use crate::{
    node::keys::NodeKeys,
    node::msg_wrapping::ElderMsgWrapping,
    node::node_ops::{AuthDuty, MessagingDuty, NodeOperation},
    utils,
};
use log::warn;
use safe_nd::{
    AppPermissions, AppPublicId, AuthCmd, AuthorisationKind, CmdError, DataAuthKind,
    Error as NdError, Message, MessageId, MiscAuthKind, MoneyAuthKind, MsgEnvelope, MsgSender,
    PublicId, QueryResponse,
};
use std::fmt::{self, Display, Formatter};

pub(super) struct Auth {
    keys: NodeKeys,
    auth_keys: AuthKeysDb,
    wrapping: ElderMsgWrapping,
}

impl Auth {
    pub fn new(keys: NodeKeys, auth_keys: AuthKeysDb, wrapping: ElderMsgWrapping) -> Self {
        Self {
            keys,
            auth_keys,
            wrapping,
        }
    }

    pub fn process(&mut self, duty: AuthDuty) -> Option<NodeOperation> {
        use AuthDuty::*;
        let result = match duty {
            Process {
                cmd,
                msg_id,
                origin,
            } => self.process_cmd(cmd, msg_id, origin),
            ListAuthKeysAndVersion { msg_id, origin, .. } => {
                self.list_auth_keys_and_version(msg_id, origin)
            }
        };
        result.map(|c| c.into())
    }

    fn list_auth_keys_and_version(
        &self,
        msg_id: MessageId,
        origin: MsgSender,
    ) -> Option<MessagingDuty> {
        let result = Ok(self.auth_keys.list_keys_and_version(&origin.id()));
        self.wrapping.send(Message::QueryResponse {
            response: QueryResponse::ListAuthKeysAndVersion(result),
            id: MessageId::new(),
            /// ID of causing query.
            correlation_id: msg_id,
            /// The sender of the causing query.
            query_origin: origin.address(),
        })
    }

    // on consensus
    fn process_cmd(
        &mut self,
        auth_cmd: AuthCmd,
        msg_id: MessageId,
        origin: MsgSender,
    ) -> Option<MessagingDuty> {
        use AuthCmd::*;
        let result = match auth_cmd {
            InsAuthKey {
                key,
                version,
                permissions,
                ..
            } => self
                .auth_keys
                .insert(&origin.id(), key, version, permissions),
            DelAuthKey { key, version, .. } => self.auth_keys.delete(&origin.id(), key, version),
        };
        if let Err(error) = result {
            return self
                .wrapping
                .error(CmdError::Auth(error), msg_id, &origin.address());
        }
        None
    }

    // Verify that valid signature is provided if the request requires it.
    pub fn verify_client_signature(&mut self, msg: &MsgEnvelope) -> Option<MessagingDuty> {
        match msg.authorisation_kind() {
            AuthorisationKind::Data(DataAuthKind::PublicRead) => None,
            _ => {
                if self.is_valid_client_signature(&msg) {
                    None
                } else {
                    self.wrapping.error(
                        CmdError::Auth(NdError::AccessDenied),
                        msg.id(),
                        &msg.origin.address(),
                    )
                }
            }
        }
    }

    fn is_valid_client_signature(&self, msg: &MsgEnvelope) -> bool {
        let signature = match &msg.origin {
            MsgSender::Client(proof) => proof.signature(),
            _ => return false,
        };
        match msg
            .origin
            .id()
            .verify(&signature, utils::serialise(&msg.message))
        {
            Ok(_) => true,
            Err(error) => {
                warn!(
                    "{}: ({:?}/{:?}) from {} is invalid: {}",
                    self,
                    "msg.get_type()",
                    msg.message.id(),
                    msg.origin.id(),
                    error
                );
                false
            }
        }
    }

    // If the client is app, check if it is authorised to perform the given request.
    pub fn authorise_app(
        &mut self,
        public_id: &PublicId,
        msg: &MsgEnvelope,
    ) -> Option<MessagingDuty> {
        let app_id = match public_id {
            PublicId::App(app_id) => app_id,
            _ => return None,
        };
        match msg.most_recent_sender() {
            MsgSender::Client { .. } => (),
            _ => return None,
        };
        let auth_kind = match &msg.message {
            Message::Cmd { cmd, .. } => cmd.authorisation_kind(),
            Message::Query { query, .. } => query.authorisation_kind(),
            _ => return None,
        };

        let result = match auth_kind {
            AuthorisationKind::Data(DataAuthKind::PublicRead) => Ok(()),
            AuthorisationKind::Data(DataAuthKind::PrivateRead) => {
                self.check_app_permissions(app_id, |_| true)
            }
            AuthorisationKind::Money(MoneyAuthKind::ReadBalance) => {
                self.check_app_permissions(app_id, |perms| perms.read_balance)
            }
            AuthorisationKind::Money(MoneyAuthKind::ReadHistory) => {
                self.check_app_permissions(app_id, |perms| perms.read_transfer_history)
            }
            AuthorisationKind::Data(DataAuthKind::Write) => {
                self.check_app_permissions(app_id, |perms| perms.data_mutations)
            }
            AuthorisationKind::Money(MoneyAuthKind::Transfer) => {
                self.check_app_permissions(app_id, |perms| perms.transfer_money)
            }
            AuthorisationKind::Misc(MiscAuthKind::WriteAndTransfer) => self
                .check_app_permissions(app_id, |perms| {
                    perms.transfer_money && perms.data_mutations
                }),
            AuthorisationKind::Misc(MiscAuthKind::ManageAppKeys) => Err(NdError::AccessDenied),
            AuthorisationKind::None => Err(NdError::AccessDenied),
        };

        if let Err(error) = result {
            return self.wrapping.error(
                CmdError::Auth(error),
                msg.message.id(),
                &msg.origin.address(),
            );
        }
        None
    }

    fn check_app_permissions(
        &self,
        app_id: &AppPublicId,
        check: impl FnOnce(AppPermissions) -> bool,
    ) -> Result<(), NdError> {
        if self
            .auth_keys
            .app_permissions(app_id)
            .map(check)
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(NdError::AccessDenied)
        }
    }
}

impl Display for Auth {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "{}", self.keys.public_key())
    }
}
