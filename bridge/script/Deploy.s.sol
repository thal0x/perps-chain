// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Script, console2} from "forge-std/Script.sol";
import {Bridge, IERC20Minimal} from "../src/Bridge.sol";
import {MockUSDC} from "../src/MockUSDC.sol";

/// Deploys MockUSDC + Bridge and writes `deployments/<DEPLOY_NAME>.json`.
///
/// Env (all optional):
///   VALIDATORS   comma-separated validator addresses (any order; sorted here).
///                Default: Anvil dev accounts 1..4.
///   POWERS       comma-separated powers, same order as VALIDATORS (default all 1).
///   EPOCH        initial validator-set epoch (default 1).
///   MIN_DEPOSIT  minimum deposit in 1e-6 USDC units (default 5_000_000 = 5 USDC).
///   MINT_AMOUNT  USDC units minted to each Anvil dev account 0..9 (default 1e12 = 1M USDC; 0 = skip).
///   DEPLOY_NAME  output file name (default "local").
///
/// Usage: see bridge/deploy.sh.
contract Deploy is Script {
    function run() external {
        address[] memory defaults = new address[](4);
        defaults[0] = 0x70997970C51812dc3A010C7d01b50e0d17dc79C8;
        defaults[1] = 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC;
        defaults[2] = 0x90F79bf6EB2c4f870365E785982E1f101E93b906;
        defaults[3] = 0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65;
        address[] memory vals = vm.envOr("VALIDATORS", ",", defaults);
        uint256[] memory ones = new uint256[](vals.length);
        for (uint256 i = 0; i < vals.length; i++) {
            ones[i] = 1;
        }
        uint256[] memory powers = vm.envOr("POWERS", ",", ones);
        require(powers.length == vals.length, "POWERS length != VALIDATORS length");
        uint64 epoch = uint64(vm.envOr("EPOCH", uint256(1)));
        uint256 minDeposit = vm.envOr("MIN_DEPOSIT", uint256(5_000_000));
        uint256 mintAmount = vm.envOr("MINT_AMOUNT", uint256(1_000_000e6));
        string memory name = vm.envOr("DEPLOY_NAME", string("local"));

        // sort (validators, powers) by address
        for (uint256 i = 1; i < vals.length; i++) {
            for (uint256 j = i; j > 0 && vals[j - 1] > vals[j]; j--) {
                (vals[j - 1], vals[j]) = (vals[j], vals[j - 1]);
                (powers[j - 1], powers[j]) = (powers[j], powers[j - 1]);
            }
        }

        vm.startBroadcast();
        MockUSDC usdc = new MockUSDC();
        Bridge bridge = new Bridge(IERC20Minimal(address(usdc)), minDeposit, vals, powers, epoch);
        if (mintAmount > 0) {
            string memory mnemonic = "test test test test test test test test test test test junk";
            for (uint32 i = 0; i < 10; i++) {
                usdc.mint(vm.addr(vm.deriveKey(mnemonic, i)), mintAmount);
            }
        }
        vm.stopBroadcast();

        string memory obj = "deployment";
        vm.serializeUint(obj, "chainId", block.chainid);
        vm.serializeUint(obj, "deployBlock", block.number);
        vm.serializeAddress(obj, "usdc", address(usdc));
        vm.serializeAddress(obj, "validators", vals);
        vm.serializeUint(obj, "powers", powers);
        vm.serializeUint(obj, "epoch", epoch);
        vm.serializeUint(obj, "minDeposit", minDeposit);
        string memory json = vm.serializeAddress(obj, "bridge", address(bridge));
        string memory path = string.concat(vm.projectRoot(), "/deployments/", name, ".json");
        vm.writeJson(json, path);
        console2.log(json);
    }
}
