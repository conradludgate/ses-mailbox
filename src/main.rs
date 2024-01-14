/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

mod aws;

use std::time::Duration;

use directory::core::config::ConfigDirectory;
use imap::core::{ImapSessionManager, IMAP};
use jmap::{api::JmapSessionManager, services::IPC_CHANNEL_BUFFER, JMAP};
use managesieve::core::ManageSieveSessionManager;
use smtp::core::{SmtpSessionManager, SMTP};
use store::config::ConfigStore;
use tokio::sync::mpsc;
use utils::{
    config::{Config, ServerProtocol},
    enable_tracing, wait_for_shutdown, UnwrapFailure,
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config = Config::init();

    // Enable tracing
    let _tracer = enable_tracing(
        &config,
        &format!(
            "Starting Stalwart IMAP Server v{}...",
            env!("CARGO_PKG_VERSION"),
        ),
    )
    .failed("Failed to enable tracing");

    // Bind ports and drop privileges
    let servers = config.parse_servers().failed("Invalid configuration");
    servers.bind(&config);

    // Parse stores and directories
    let stores = config.parse_stores().await.failed("Invalid configuration");
    let directory = config
        .parse_directory(&stores, config.value("jmap.store.data"))
        .await
        .failed("Invalid configuration");
    let schedulers = config
        .parse_purge_schedules(
            &stores,
            config.value("jmap.store.data"),
            config.value("jmap.store.blob"),
        )
        .await
        .failed("Invalid configuration");

    // Init servers
    let (delivery_tx, delivery_rx) = mpsc::channel(IPC_CHANNEL_BUFFER);
    let smtp = SMTP::init(&config, &servers, &stores, &directory, delivery_tx.clone())
        .await
        .failed("Invalid configuration file");
    let jmap = JMAP::init(&config, &stores, &directory, delivery_rx, smtp.clone())
        .await
        .failed("Invalid configuration file");
    let imap = IMAP::init(&config)
        .await
        .failed("Invalid configuration file");

    let queue_url = std::env::var("SES_MAILBOX_QUEUE")
        .failed("SES_MAILBOX_QUEUE environment var should be provided");

    tokio::spawn(aws::ingest(queue_url, delivery_tx));

    // Spawn servers
    let (shutdown_tx, shutdown_rx) = servers.spawn(|server, shutdown_rx| {
        match &server.protocol {
            ServerProtocol::Smtp | ServerProtocol::Lmtp => {
                server.spawn(SmtpSessionManager::new(smtp.clone()), shutdown_rx)
            }
            ServerProtocol::Http => {
                tracing::debug!("Ignoring HTTP server listener, using JMAP port instead.");
            }
            ServerProtocol::Jmap => {
                server.spawn(JmapSessionManager::new(jmap.clone()), shutdown_rx)
            }
            ServerProtocol::Imap => server.spawn(
                ImapSessionManager::new(jmap.clone(), imap.clone()),
                shutdown_rx,
            ),
            ServerProtocol::ManageSieve => server.spawn(
                ManageSieveSessionManager::new(jmap.clone(), imap.clone()),
                shutdown_rx,
            ),
        };
    });

    // Spawn purge schedulers
    for scheduler in schedulers {
        scheduler.spawn(shutdown_rx.clone());
    }

    // Wait for shutdown signal
    wait_for_shutdown(&format!(
        "Shutting down Stalwart IMAP Server v{}...",
        env!("CARGO_PKG_VERSION")
    ))
    .await;

    // Stop services
    let _ = shutdown_tx.send(true);

    // Wait for services to finish
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}
