//! Sarun adapters for Chupa's archive gateway extension seams.

pub use chupa::gateway::{browser_base_url, host_base_url};

struct SarunCaptures;

impl chupa::gateway::CaptureProvider for SarunCaptures {
    fn archives(&self) -> Result<Vec<chupa::gateway::CaptureArchive>, String> {
        Ok(crate::discover::discover()
            .into_iter()
            .map(|(id, session)| chupa::gateway::CaptureArchive {
                id,
                name: session.name,
            })
            .collect())
    }

    fn rows(&self, archive: i64) -> Result<Vec<chupa::gateway::CaptureRow>, String> {
        crate::discover::webcap_typed(archive)?
            .into_iter()
            .map(|row| {
                Ok(chupa::gateway::CaptureRow {
                    id: row.id,
                    status: row.status,
                    url: row.url.as_str().to_string(),
                    mime: row.mime.as_str().to_string(),
                    response_length: row.response_length,
                })
            })
            .collect()
    }

    fn detail(
        &self,
        archive: i64,
        row: u64,
    ) -> Result<Option<chupa::gateway::CaptureDetail>, String> {
        Ok(crate::discover::webcap_detail_typed(archive, row)?.map(|capture| {
            chupa::gateway::CaptureDetail {
                status: capture.summary.status,
                mime: capture.summary.mime.as_str().to_string(),
                response_body: capture.response_body.as_slice().to_vec(),
            }
        }))
    }
}

struct SarunEnhancer(crate::realplksr::Enhancer);

impl chupa::gateway::ImageEnhancer for SarunEnhancer {
    fn cached(&self, route: &str) -> Option<Vec<u8>> {
        self.0.cached(route).map(|bytes| bytes.as_ref().clone())
    }

    fn image(&self, route: &str, mime: &str, body: &[u8]) -> chupa::gateway::Enhancement {
        match self.0.image(route, mime, body) {
            crate::realplksr::Enhancement::Ready(bytes) => {
                chupa::gateway::Enhancement::Ready(bytes.as_ref().clone())
            }
            crate::realplksr::Enhancement::Pending => chupa::gateway::Enhancement::Pending,
            crate::realplksr::Enhancement::Original => chupa::gateway::Enhancement::Original,
        }
    }
}

pub struct Gateway(chupa::gateway::Gateway);

impl Gateway {
    pub fn start(self_exe: String) -> Result<Self, String> {
        chupa::gateway::Gateway::start_with(
            self_exe,
            chupa::gateway::GatewayServices {
                captures: std::sync::Arc::new(SarunCaptures),
                enhancer: std::sync::Arc::new(SarunEnhancer(crate::realplksr::Enhancer::new())),
            },
        )
        .map(Self)
    }

    pub fn shutdown(self) {
        self.0.shutdown();
    }
}
