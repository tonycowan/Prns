use crate::engine::{
    PathFound, PathRequestId, PrnsCommand, RequestPath, Settlement, PATH_REQUEST_ID_LEN,
};
use crate::interfaces::InterfaceId;
use crate::wire::DestinationHash;

use super::{PrnsNodeHandle, RequestPathError};

impl PrnsNodeHandle {
    pub async fn request_path(
        &self,
        destination: DestinationHash,
    ) -> Result<PathFound, RequestPathError> {
        self.request_path_on(destination, None).await
    }

    /// Asks for a path on one interface. `None` asks on every interface that is up.
    pub async fn request_path_on(
        &self,
        destination: DestinationHash,
        only_interface: Option<InterfaceId>,
    ) -> Result<PathFound, RequestPathError> {
        let mut request_id = [0; PATH_REQUEST_ID_LEN];
        self.entropy
            .try_fill_path_id(&mut request_id)
            .map_err(|_| RequestPathError::EntropyUnavailable)?;
        let timing = self.path_command_timing().await;
        match self
            .settle_with_timing(
                PrnsCommand::RequestPath(RequestPath {
                    destination,
                    id: PathRequestId::new(request_id),
                    only_interface,
                }),
                timing,
            )
            .await
        {
            Some(Settlement::RequestPath(result)) => result.map_err(RequestPathError::Failed),
            Some(_) | None => Err(RequestPathError::NodeStopped),
        }
    }
}

#[cfg(test)]
mod tests;
