use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use strum::Display;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::{config::CONFIG, server::clore::Clore};

use super::ssh;

lazy_static::lazy_static! {
    pub static ref WALLETS_STATE:Arc<Mutex<Address>> = {
        Arc::new(Mutex::new(Address::default()))
    };
}

#[derive(Debug, Display, PartialEq, Clone, Serialize, Deserialize)]
pub enum AddressType {
    MASTER,
    SUB,
    NULL,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub address: String,
    pub addr_type: AddressType,
    pub start_time: Option<DateTime<Local>>,
    pub report_last_time: Option<DateTime<Local>>,
    pub deploy: Deployed,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum Deployed {
    NOTASSIGNED,
    DEPLOYING {
        orderid: u32,
        serverid:u32,
        sshaddr: Option<String>,
        sshport: Option<u16>,
    },
    DEPLOYED {
        orderid: u32,
        serverid:u32,
        sshaddr: Option<String>,
        sshport: Option<u16>,
    },
}

impl Wallet {
    pub fn new(address: String, addr_type: AddressType) -> Wallet {
        Wallet {
            address,
            addr_type,
            start_time: None,
            report_last_time: None,
            deploy: Deployed::NOTASSIGNED,
        }
    }
}

#[derive(PartialEq, Debug)]
pub struct Address(pub HashMap<String, Wallet>);

impl std::ops::DerefMut for Address {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl std::ops::Deref for Address {
    type Target = HashMap<String, Wallet>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for Address {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl Address {
    async fn mstaddress(address: &str) -> AddressType {
        let url = "https://mainnet.nimble.technology/check_balance";
        let result = Address::curl(url, address).await;
        if let Err(_) = result {
            return AddressType::NULL;
        }
        let text = result.unwrap();
        if text.contains("Error") {
            AddressType::NULL
        } else {
            AddressType::MASTER
        }
    }

    async fn subaddress(address: &str) -> AddressType {
        let url = "https://mainnet.nimble.technology/register_particle";
        let result = Address::curl(url, address).await;
        if let Err(_) = result {
            return AddressType::NULL;
        }
        let text = result.unwrap();
        if text.contains("Task registered successfully") {
            AddressType::SUB
        } else {
            AddressType::NULL
        }
    }

    async fn curl(url: &str, address: &str) -> Result<String, String> {
        info!("网络请求:{},{}", url, address);
        let mut params = HashMap::new();
        params.insert("address", address);
        let client = reqwest::Client::new();
        let result = client
            .post(url)
            .json(&params)
            .send()
            .await
            .map_err(|e| e.to_string());
        if let Err(msg) = &result {
            warn!("发起网络请求失败:{}", msg);
            return Err(msg.clone());
        }

        let response = result.unwrap();

        info!("远程响应状态码:{}", response.status());

        let result = response.text().await.map_err(|e| e.to_string());
        if let Err(msg) = &result {
            warn!(msg);
            return Err(msg.clone());
        }
        let text = &result.unwrap();
        info!("远程响应结果:{}", text);
        Ok(text.to_string())
    }

    pub async fn load_address_file() -> Vec<Wallet> {
        let mutex_conf = Arc::clone(&CONFIG);
        let config = &mutex_conf.lock().await;
        config
            .wallet
            .address
            .iter()
            .map(|address| Wallet::new(address.clone(), AddressType::NULL))
            .collect::<Vec<Wallet>>()
    }

    pub async fn check(&mut self, other_wallets: &Vec<Wallet>) {
        for wallet in other_wallets.iter() {
            let address = wallet.address.clone();
            if (*self).contains_key(&address) {
                let wallet = self.get(&address).unwrap();
                info!(
                    "地址:{:?}已被检测过,地址角色:{:?}",
                    &address, wallet.addr_type
                );
                continue;
            }

            let (subaddress, mstaddress) =
                tokio::join!(Address::subaddress(&address), Address::mstaddress(&address));
            info!("地址检测结果:{:?},{:?}", mstaddress, subaddress);
            let addr_type = if let AddressType::MASTER = mstaddress {
                AddressType::MASTER
            } else if let AddressType::SUB = subaddress {
                AddressType::SUB
            } else {
                AddressType::NULL
            };
            if addr_type != AddressType::NULL {
                (*self).insert(
                    address.clone(),
                    Wallet::new(address.clone(), addr_type.clone()),
                );
            }
            info!("地址匹配结果:{:?}", addr_type.clone());
        }
    }

    // 过滤规则
    // 未分配订单id的服务器
    pub async fn filter(&mut self) -> Vec<Wallet> {
        let mut wallets: Vec<Wallet> = Vec::new();
        let result = Clore::default().my_orders().await;
        if let Ok(orders) = result {
            let (lists, error) = ssh::Ssh::try_run_command_remote(&orders).await;
            if !error.is_empty() {
                return wallets;
            }
            for (address, deployed) in lists {
                for wallet in wallets.iter_mut() {
                    if wallet.address == address {
                        wallet.deploy = deployed.clone();
                    }
                }
            }
        }

        for (_, wallet) in (*self).iter_mut() {
            if wallet.addr_type == AddressType::SUB && wallet.deploy == Deployed::NOTASSIGNED {
                wallets.push(wallet.clone());
            }
        }

        wallets
    }

    // 分配服务器
    pub async fn assgin_server(
        &mut self,
        wallet_adress: &str,
        deploy:Deployed
    ) -> Result<(), String> {
        if !(*self).contains_key(wallet_adress) {
            return Err("不存在钱包地址！".to_string());
        }
        let local_time = Local::now();
        let wallet = (*self).get_mut(wallet_adress).unwrap();
        if Deployed::NOTASSIGNED == wallet.deploy {
            wallet.deploy = deploy;
            wallet.start_time = Some(local_time);
            Ok(())
        } else {
            Err("当前地址状态不是待分配状态！".to_string())
        }
    }

    pub async fn update_log_collect_time(&mut self, wallet_adress: &str) -> bool {
        if !(*self).contains_key(wallet_adress) {
            return false;
        }
        let wallet = (*self).get_mut(wallet_adress).unwrap();
        if let Deployed::DEPLOYING {
            orderid,
            serverid,
            sshaddr,
            sshport,
        } = &wallet.deploy
        {
            let local_time = Local::now();
            wallet.report_last_time = Some(local_time);
            wallet.deploy = Deployed::DEPLOYED {
                orderid: orderid.clone(),
                serverid:serverid.clone(),
                sshaddr: sshaddr.clone(),
                sshport: sshport.clone(),
            };
        }

        true
    }

    // 超时未上报时间，则取消该机器订单号，重置所有钱包信息
    pub async fn filter_log_timeout(&mut self, clore: &Clore) {
        let mut order_ids: Vec<u32> = Vec::new();
        for (_, wallet) in (*self).iter_mut() {
            let nowtime = Local::now();
            match &wallet.deploy {
                Deployed::NOTASSIGNED => {}
                Deployed::DEPLOYING { orderid, .. } => {
                    // 创建时间超过15分钟，还未有上报时间则，进行取消订单
                    if let Some(start_time) = wallet.start_time {
                        if nowtime.timestamp() - start_time.timestamp() > 15 * 60 {
                            order_ids.push(orderid.clone());
                        }
                    }
                }
                Deployed::DEPLOYED { orderid, .. } => {
                    // 上报时间若是超过了十分钟，则也取消，订单号
                    if let Some(report_last_time) = wallet.report_last_time {
                        if nowtime.timestamp() - report_last_time.timestamp() > 10 * 60 {
                            order_ids.push(orderid.clone());
                        }
                    }
                }
            }
        }
        for order_id in order_ids.iter() {
            let result = clore.cancel_order(order_id.clone()).await;
            if let Err(e) = result {
                error!("订单:{:?}取消失败,错误码：{:?}", order_id, e);
            } else {
                warn!("已取消{:?}该订单", order_id);
            }
        }
    }
}

pub async fn pool() {
    loop {
        let wallets = Arc::clone(&WALLETS_STATE);
        let mut locked = wallets.lock().await;
        let other = Address::load_address_file().await;
        locked.check(&other).await;
        let wallets = locked.filter().await;
        info!("当前绑定信息:{:?}", *locked);
        // let address = wallets
        //     .iter()
        //     .map(|wallet| wallet.address.to_string())
        //     .collect::<Vec<String>>();

        if wallets.len() > 0 {
            // warn!("待分配地址:\n{}", address.join("\n"));
            // let market = Clore::default().marketplace().await;
            // if let Ok(cards) = market {
            //     let server_ids = cards
            //         .iter()
            //         .filter(|item| item.card_number == 2)
            //         .map(|item| item.server_id)
            //         .collect::<Vec<u32>>();
            //     info!("server_ids:{:?}", server_ids);
            // }
        }
        drop(locked);
        tokio::time::sleep(std::time::Duration::from_secs(60 * 5)).await;
    }
}