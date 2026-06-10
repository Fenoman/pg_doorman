#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResetCleanupCommand {
    /// A client statement that PostgreSQL reports as `RESET\0` and that resets
    /// every GUC tracked by checkin-time `RESET ALL`.
    ResetAll,
    /// `RESET ROLE` restores the current role but does not reset session
    /// authorization or ordinary GUCs.
    ResetRole,
    /// `RESET SESSION AUTHORIZATION` restores session authorization and current
    /// role but does not reset ordinary GUCs.
    ResetSessionAuthorization,
    /// Any other successful `RESET ...` statement. PostgreSQL reports these with
    /// the same `RESET\0` tag, but they cannot prove unrelated GUCs were reset.
    PerGucReset,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetCleanupCommand {
    /// Ordinary `SET ...` session GUC.
    GenericSet,
    /// `SET ROLE <role>`.
    SetRole,
    /// `SET ROLE DEFAULT` / `SET ROLE NONE`.
    SetRoleDefault,
    /// `SET SESSION AUTHORIZATION <role>`.
    SetSessionAuthorization,
    /// `SET SESSION AUTHORIZATION DEFAULT`.
    SetSessionAuthorizationDefault,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct CleanupState {
    /// If server connection requires RESET ALL before checkin because of set statement
    pub(crate) needs_cleanup_set: bool,

    /// If server connection requires RESET ROLE before checkin because of SET ROLE
    pub(crate) needs_cleanup_role: bool,

    /// If server connection requires RESET SESSION AUTHORIZATION before checkin
    /// because of SET SESSION AUTHORIZATION
    pub(crate) needs_cleanup_session_authorization: bool,

    /// If server connection requires DEALLOCATE ALL before checkin because of prepare statement
    pub(crate) needs_cleanup_prepare: bool,

    /// If server connection requires CLOSE ALL before checkin because of declare statement
    pub(crate) needs_cleanup_declare: bool,
}

impl CleanupState {
    pub(crate) fn new() -> Self {
        CleanupState {
            needs_cleanup_set: false,
            needs_cleanup_role: false,
            needs_cleanup_session_authorization: false,
            needs_cleanup_prepare: false,
            needs_cleanup_declare: false,
        }
    }

    #[inline(always)]
    pub(crate) fn needs_cleanup(&self) -> bool {
        self.needs_cleanup_set
            || self.needs_cleanup_role
            || self.needs_cleanup_session_authorization
            || self.needs_cleanup_prepare
            || self.needs_cleanup_declare
    }

    #[inline(always)]
    pub(crate) fn set_true(&mut self) {
        self.needs_cleanup_set = true;
        self.needs_cleanup_role = true;
        self.needs_cleanup_session_authorization = true;
        self.needs_cleanup_prepare = true;
        self.needs_cleanup_declare = true;
    }

    #[inline(always)]
    pub(crate) fn reset(&mut self) {
        self.needs_cleanup_set = false;
        self.needs_cleanup_role = false;
        self.needs_cleanup_session_authorization = false;
        self.needs_cleanup_prepare = false;
        self.needs_cleanup_declare = false;
    }
}

impl std::fmt::Display for CleanupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SET: {}, ROLE: {}, SESSION_AUTHORIZATION: {}, PREPARE: {}, DECLARE: {}",
            self.needs_cleanup_set,
            self.needs_cleanup_role,
            self.needs_cleanup_session_authorization,
            self.needs_cleanup_prepare,
            self.needs_cleanup_declare
        )
    }
}
