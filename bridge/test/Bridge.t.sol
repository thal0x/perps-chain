// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Bridge, IERC20Minimal} from "../src/Bridge.sol";
import {MockUSDC} from "../src/MockUSDC.sol";

contract BridgeTestBase is Test {
    uint256 internal constant HALF_N = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;
    uint256 internal constant N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;

    MockUSDC internal usdc;
    Bridge internal bridge;

    // current validator set: keys sorted by address
    uint256[] internal keys;
    address[] internal vals;
    uint256[] internal powers;

    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);

    uint256 internal constant MIN_DEPOSIT = 5_000_000; // 5 USDC

    function setUp() public virtual {
        usdc = new MockUSDC();
        (keys, vals) = _makeSet(100, 4);
        powers = _equalPowers(4, 1);
        bridge = new Bridge(IERC20Minimal(address(usdc)), MIN_DEPOSIT, vals, powers, 1);
        usdc.mint(address(bridge), 1_000_000e6);
        usdc.mint(alice, 10_000e6);
    }

    // ---------------------------------------------------------------- helpers
    /// `n` keys starting at seed, sorted by address.
    function _makeSet(uint256 seed, uint256 n) internal pure returns (uint256[] memory ks, address[] memory as_) {
        ks = new uint256[](n);
        as_ = new address[](n);
        for (uint256 i = 0; i < n; i++) {
            ks[i] = uint256(keccak256(abi.encode("validator", seed + i))) % (N - 1) + 1;
            as_[i] = vm.addr(ks[i]);
        }
        // insertion sort by address
        for (uint256 i = 1; i < n; i++) {
            for (uint256 j = i; j > 0 && as_[j - 1] > as_[j]; j--) {
                (as_[j - 1], as_[j]) = (as_[j], as_[j - 1]);
                (ks[j - 1], ks[j]) = (ks[j], ks[j - 1]);
            }
        }
    }

    function _equalPowers(uint256 n, uint256 p) internal pure returns (uint256[] memory ps) {
        ps = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            ps[i] = p;
        }
    }

    function _sign(uint256 key, bytes32 digest) internal pure returns (Bridge.Sig memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return Bridge.Sig(v, r, s);
    }

    /// Sign with the keys at the given indices (caller controls order).
    function _signIdx(uint256[] memory ks, uint256[] memory idx, bytes32 digest)
        internal
        pure
        returns (Bridge.Sig[] memory sigs)
    {
        sigs = new Bridge.Sig[](idx.length);
        for (uint256 i = 0; i < idx.length; i++) {
            sigs[i] = _sign(ks[idx[i]], digest);
        }
    }

    function _firstK(uint256 k) internal pure returns (uint256[] memory idx) {
        idx = new uint256[](k);
        for (uint256 i = 0; i < k; i++) {
            idx[i] = i;
        }
    }

    function _w(uint64 id, address dest, uint256 amount) internal pure returns (Bridge.Withdrawal memory) {
        return Bridge.Withdrawal(id, dest, amount);
    }

    function _digest(Bridge.Withdrawal memory w) internal view returns (bytes32) {
        return bridge.hashWithdrawal(w);
    }

    function _rotate(
        uint256[] memory signerKeys,
        address[] memory newVals,
        uint256[] memory newPowers,
        uint64 newEpoch
    ) internal {
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, newPowers, newEpoch);
        bridge.updateValidatorSet(newVals, newPowers, newEpoch, _signIdx(signerKeys, _firstK(signerKeys.length), d));
    }
}

contract BridgeDepositTest is BridgeTestBase {
    function test_Deposit_EmitsAndAssignsMonotoneIds() public {
        vm.startPrank(alice);
        usdc.approve(address(bridge), type(uint256).max);
        uint256 before = usdc.balanceOf(address(bridge));
        vm.expectEmit(true, true, false, true, address(bridge));
        emit Bridge.Deposit(0, alice, 1000e6);
        assertEq(bridge.deposit(1000e6), 0);
        vm.expectEmit(true, true, false, true, address(bridge));
        emit Bridge.Deposit(1, alice, 5e6);
        assertEq(bridge.deposit(5e6), 1);
        vm.stopPrank();
        assertEq(bridge.nextDepositId(), 2);
        assertEq(usdc.balanceOf(address(bridge)), before + 1005e6);
        assertEq(usdc.balanceOf(alice), 10_000e6 - 1005e6);
    }

    function test_Deposit_BelowMinimumReverts() public {
        vm.startPrank(alice);
        usdc.approve(address(bridge), type(uint256).max);
        vm.expectRevert(Bridge.DepositTooSmall.selector);
        bridge.deposit(MIN_DEPOSIT - 1);
        vm.expectRevert(Bridge.DepositTooSmall.selector);
        bridge.deposit(0);
        bridge.deposit(MIN_DEPOSIT); // exact minimum is fine
        vm.stopPrank();
    }

    function test_Deposit_TooLargeReverts() public {
        uint256 big = uint256(type(uint64).max >> 1) + 1;
        usdc.mint(alice, big);
        vm.startPrank(alice);
        usdc.approve(address(bridge), type(uint256).max);
        vm.expectRevert(Bridge.DepositTooLarge.selector);
        bridge.deposit(big);
        vm.stopPrank();
    }

    function test_Deposit_WithoutApprovalReverts() public {
        vm.prank(alice);
        vm.expectRevert(Bridge.TransferFailed.selector);
        bridge.deposit(10e6);
        assertEq(bridge.nextDepositId(), 0);
    }

    function test_Deposit_InsufficientBalanceReverts() public {
        vm.startPrank(bob);
        usdc.approve(address(bridge), type(uint256).max);
        vm.expectRevert(Bridge.TransferFailed.selector);
        bridge.deposit(10e6);
        vm.stopPrank();
    }

    function testFuzz_Deposit(uint256 amount, uint8 count) public {
        amount = bound(amount, MIN_DEPOSIT, 1_000e6);
        count = uint8(bound(count, 1, 10));
        usdc.mint(bob, amount * count);
        vm.startPrank(bob);
        usdc.approve(address(bridge), type(uint256).max);
        for (uint64 i = 0; i < count; i++) {
            vm.expectEmit(true, true, false, true, address(bridge));
            emit Bridge.Deposit(i, bob, amount);
            assertEq(bridge.deposit(amount), i);
        }
        vm.stopPrank();
        assertEq(usdc.balanceOf(bob), 0);
    }

    function test_Constructor_RejectsBadSets() public {
        address[] memory v = new address[](2);
        uint256[] memory p = _equalPowers(2, 1);
        v[0] = address(2);
        v[1] = address(1); // unsorted
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        new Bridge(IERC20Minimal(address(usdc)), 1, v, p, 0);
        v[1] = address(2); // duplicate
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        new Bridge(IERC20Minimal(address(usdc)), 1, v, p, 0);
        v[0] = address(0); // zero
        v[1] = address(1);
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        new Bridge(IERC20Minimal(address(usdc)), 1, v, p, 0);
        v[0] = address(1);
        v[1] = address(2);
        p[1] = 0; // zero power
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        new Bridge(IERC20Minimal(address(usdc)), 1, v, p, 0);
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        new Bridge(IERC20Minimal(address(usdc)), 1, new address[](0), new uint256[](0), 0);
        vm.expectRevert(Bridge.InvalidToken.selector);
        new Bridge(IERC20Minimal(address(0xdead)), 1, vals, powers, 0);
    }

    function test_Views() public view {
        (address[] memory v, uint256[] memory p) = bridge.getValidators();
        assertEq(v.length, 4);
        for (uint256 i = 0; i < 4; i++) {
            assertEq(v[i], vals[i]);
            assertEq(p[i], 1);
        }
        assertEq(bridge.totalPower(), 4);
        assertEq(bridge.quorumPower(), 3);
        assertEq(bridge.epoch(), 1);
        assertEq(address(bridge.token()), address(usdc));
        assertEq(bridge.minDeposit(), MIN_DEPOSIT);
    }
}

contract BridgeSignatureThresholdTest is BridgeTestBase {
    function _expectRevertWith(uint256[] memory idx, bytes4 sel) internal {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, idx, _digest(w));
        vm.expectRevert(sel);
        bridge.withdraw(w, sigs);
    }

    /// 4 equal validators: quorum = floor(8/3)+1 = 3.
    function test_SignatureThreshold_ExactQuorumSucceeds() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        vm.expectEmit(true, true, false, true, address(bridge));
        emit Bridge.Withdrawn(7, bob, 499e6);
        bridge.withdraw(w, _signIdx(keys, _firstK(3), _digest(w)));
        assertEq(usdc.balanceOf(bob), 499e6);
        assertTrue(bridge.usedWithdrawal(7));
    }

    function test_SignatureThreshold_AllSignersSucceeds() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        bridge.withdraw(w, _signIdx(keys, _firstK(4), _digest(w)));
        assertEq(usdc.balanceOf(bob), 499e6);
    }

    function test_SignatureThreshold_BelowQuorumReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(2), _digest(w));
        vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, 2, 3));
        bridge.withdraw(w, sigs);
        vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, 0, 3));
        bridge.withdraw(w, new Bridge.Sig[](0));
    }

    /// Exactly 2/3 of the power is NOT enough (strictly more than 2/3 required).
    function test_SignatureThreshold_ExactlyTwoThirdsIsNotEnough() public {
        (uint256[] memory ks, address[] memory as_) = _makeSet(500, 3);
        Bridge b = new Bridge(IERC20Minimal(address(usdc)), 1, as_, _equalPowers(3, 1), 0);
        usdc.mint(address(b), 100e6);
        assertEq(b.quorumPower(), 3);
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        bytes32 d = b.hashWithdrawal(w);
        Bridge.Sig[] memory two = _signIdx(ks, _firstK(2), d);
        vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, 2, 3));
        b.withdraw(w, two);
        b.withdraw(w, _signIdx(ks, _firstK(3), d));
        assertEq(usdc.balanceOf(bob), 1e6);
    }

    /// Weighted powers: total 10 => quorum 7. Power 7 passes, 6 fails.
    function test_SignatureThreshold_WeightedBoundary() public {
        (uint256[] memory ks, address[] memory as_) = _makeSet(900, 4);
        uint256[] memory p = new uint256[](4);
        p[0] = 4;
        p[1] = 3;
        p[2] = 2;
        p[3] = 1;
        Bridge b = new Bridge(IERC20Minimal(address(usdc)), 1, as_, p, 0);
        usdc.mint(address(b), 100e6);
        assertEq(b.quorumPower(), 7);
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        bytes32 d = b.hashWithdrawal(w);
        uint256[] memory idx = new uint256[](3);
        idx[0] = 0;
        idx[1] = 2;
        idx[2] = 3; // 4 + 2 + 1 = 7
        uint256[] memory idx6 = new uint256[](2);
        idx6[0] = 0;
        idx6[1] = 2; // 6
        vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, 6, 7));
        b.withdraw(w, _signIdx(ks, idx6, d));
        b.withdraw(w, _signIdx(ks, idx, d));
        assertEq(usdc.balanceOf(bob), 1e6);
    }

    function test_SignatureThreshold_DuplicateSignerReverts() public {
        uint256[] memory idx = new uint256[](3);
        idx[0] = 0;
        idx[1] = 1;
        idx[2] = 1;
        _expectRevertWith(idx, Bridge.SignersNotSorted.selector);
    }

    function test_SignatureThreshold_UnsortedSignersReverts() public {
        uint256[] memory idx = new uint256[](3);
        idx[0] = 1;
        idx[1] = 0;
        idx[2] = 2;
        _expectRevertWith(idx, Bridge.SignersNotSorted.selector);
    }

    function test_SignatureThreshold_NonValidatorSignerReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        bytes32 d = _digest(w);
        (uint256[] memory outsiders,) = _makeSet(7777, 1);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), d);
        // replace one signature with an outsider's, keep sorted order valid
        Bridge.Sig[] memory four = new Bridge.Sig[](4);
        address o = vm.addr(outsiders[0]);
        uint256 k = 0;
        bool placed = false;
        for (uint256 i = 0; i < 3; i++) {
            if (!placed && o < vals[i]) {
                four[k++] = _sign(outsiders[0], d);
                placed = true;
            }
            four[k++] = sigs[i];
        }
        if (!placed) four[k++] = _sign(outsiders[0], d);
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.withdraw(w, four);
        // Outsider alone
        Bridge.Sig[] memory one = new Bridge.Sig[](1);
        one[0] = _sign(outsiders[0], d);
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.withdraw(w, one);
    }

    function test_SignatureThreshold_MalleableHighSReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), _digest(w));
        // (r, n - s, v ^ 1) recovers the same signer but is rejected (EIP-2)
        Bridge.Sig memory s0 = sigs[0];
        sigs[0] = Bridge.Sig(s0.v == 27 ? 28 : 27, s0.r, bytes32(N - uint256(s0.s)));
        assertEq(ecrecover(_digest(w), sigs[0].v, sigs[0].r, sigs[0].s), vals[0]);
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
    }

    function test_SignatureThreshold_MalformedSigReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), _digest(w));
        Bridge.Sig memory good = Bridge.Sig(sigs[0].v, sigs[0].r, sigs[0].s);
        sigs[0].v = 29; // bad v
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
        sigs[0] = Bridge.Sig(good.v, good.r, good.s);
        sigs[0].v = 0; // 0/1 not accepted on-chain (relayer normalises)
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
        sigs[0] = Bridge.Sig(good.v, good.r, good.s);
        sigs[0].r = bytes32(0);
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
        sigs[0] = Bridge.Sig(good.v, good.r, good.s);
        sigs[0].s = bytes32(0);
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
        // r not on curve (x >= p) => ecrecover returns 0
        sigs[0] = Bridge.Sig(good.v, good.r, good.s);
        sigs[0].r = bytes32(type(uint256).max);
        vm.expectRevert(Bridge.InvalidSignature.selector);
        bridge.withdraw(w, sigs);
        sigs[0] = Bridge.Sig(good.v, good.r, good.s);
        bridge.withdraw(w, sigs);
    }

    function test_SignatureThreshold_SignatureOverOtherMessageReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 499e6);
        // signed amount differs: recovers to random addresses -> NotValidator or unsorted
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), _digest(_w(7, bob, 500e6)));
        vm.expectRevert();
        bridge.withdraw(w, sigs);
        // signed destination differs
        sigs = _signIdx(keys, _firstK(4), _digest(_w(7, alice, 499e6)));
        vm.expectRevert();
        bridge.withdraw(w, sigs);
        assertEq(usdc.balanceOf(bob), 0);
    }

    function test_SignatureThreshold_InvalidWithdrawalReverts() public {
        Bridge.Withdrawal memory w = _w(7, address(0), 499e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), _digest(w));
        vm.expectRevert(Bridge.InvalidWithdrawal.selector);
        bridge.withdraw(w, sigs);
        w = _w(8, bob, 0);
        sigs = _signIdx(keys, _firstK(4), _digest(w));
        vm.expectRevert(Bridge.InvalidWithdrawal.selector);
        bridge.withdraw(w, sigs);
    }

    function test_SignatureThreshold_InsufficientBridgeBalanceReverts() public {
        Bridge.Withdrawal memory w = _w(7, bob, 2_000_000e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), _digest(w));
        vm.expectRevert(Bridge.TransferFailed.selector);
        bridge.withdraw(w, sigs);
        assertFalse(bridge.usedWithdrawal(7));
    }

    /// Fuzz: random subsets of a random-sized weighted set pass iff power >= floor(2T/3)+1.
    function testFuzz_SignatureThreshold_Subsets(uint8 n, uint256 mask, uint256 powerSeed) public {
        n = uint8(bound(n, 1, 10));
        (uint256[] memory ks, address[] memory as_) = _makeSet(uint256(keccak256(abi.encode(powerSeed))), n);
        uint256[] memory p = new uint256[](n);
        uint256 total = 0;
        for (uint256 i = 0; i < n; i++) {
            p[i] = uint256(keccak256(abi.encode(powerSeed, i))) % 100 + 1;
            total += p[i];
        }
        Bridge b = new Bridge(IERC20Minimal(address(usdc)), 1, as_, p, 0);
        usdc.mint(address(b), 1e6);
        Bridge.Withdrawal memory w = _w(3, bob, 1e6);
        bytes32 d = b.hashWithdrawal(w);
        uint256 cnt = 0;
        for (uint256 i = 0; i < n; i++) {
            if ((mask >> i) & 1 == 1) cnt++;
        }
        uint256[] memory idx = new uint256[](cnt);
        uint256 have = 0;
        uint256 k = 0;
        for (uint256 i = 0; i < n; i++) {
            if ((mask >> i) & 1 == 1) {
                idx[k++] = i;
                have += p[i];
            }
        }
        Bridge.Sig[] memory sigs = _signIdx(ks, idx, d);
        uint256 need = total * 2 / 3 + 1;
        assertEq(b.quorumPower(), need);
        // strictly more than 2/3: 3 * have > 2 * total
        if (3 * have > 2 * total) {
            assertGe(have, need);
            b.withdraw(w, sigs);
            assertEq(usdc.balanceOf(bob), 1e6);
        } else {
            vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, have, need));
            b.withdraw(w, sigs);
        }
    }

    /// Fuzz: arbitrary garbage signatures never authorize a withdrawal.
    function testFuzz_SignatureThreshold_GarbageSigs(uint8 v, bytes32 r, bytes32 s) public {
        Bridge.Withdrawal memory w = _w(9, bob, 1e6);
        Bridge.Sig[] memory sigs = new Bridge.Sig[](3);
        for (uint256 i = 0; i < 3; i++) {
            sigs[i] = Bridge.Sig(v, r, keccak256(abi.encode(s, i)));
        }
        vm.expectRevert();
        bridge.withdraw(w, sigs);
        assertEq(usdc.balanceOf(bob), 0);
    }
}

contract BridgeReplayTest is BridgeTestBase {
    function test_ReplayProtection_SameIdRejected() public {
        Bridge.Withdrawal memory w = _w(42, bob, 10e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), _digest(w));
        bridge.withdraw(w, sigs);
        vm.expectRevert(Bridge.WithdrawalAlreadyUsed.selector);
        bridge.withdraw(w, sigs);
        assertEq(usdc.balanceOf(bob), 10e6);
    }

    function test_ReplayProtection_SameIdDifferentPayloadRejected() public {
        Bridge.Withdrawal memory w = _w(42, bob, 10e6);
        bridge.withdraw(w, _signIdx(keys, _firstK(3), _digest(w)));
        // even a fully-signed, different withdrawal with the same id is rejected
        Bridge.Withdrawal memory w2 = _w(42, alice, 20e6);
        Bridge.Sig[] memory sigs2 = _signIdx(keys, _firstK(4), _digest(w2));
        vm.expectRevert(Bridge.WithdrawalAlreadyUsed.selector);
        bridge.withdraw(w2, sigs2);
    }

    function test_ReplayProtection_DifferentIdsIndependent() public {
        for (uint64 id = 0; id < 5; id++) {
            Bridge.Withdrawal memory w = _w(id, bob, 1e6);
            bridge.withdraw(w, _signIdx(keys, _firstK(3), _digest(w)));
        }
        assertEq(usdc.balanceOf(bob), 5e6);
    }

    function test_ReplayProtection_SignaturesBoundToContractAndChain() public {
        // A second bridge with the same validators must reject signatures made for the first.
        Bridge other = new Bridge(IERC20Minimal(address(usdc)), 1, vals, powers, 1);
        usdc.mint(address(other), 100e6);
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), _digest(w));
        assertTrue(other.hashWithdrawal(w) != bridge.hashWithdrawal(w));
        vm.expectRevert();
        other.withdraw(w, sigs);
        // Different chain id => different domain => signatures invalid
        bytes32 before = bridge.hashWithdrawal(w);
        vm.chainId(1);
        assertTrue(bridge.hashWithdrawal(w) != before);
        vm.expectRevert();
        bridge.withdraw(w, sigs);
    }

    function test_ReplayProtection_WithdrawalSigCannotRotateSet() public {
        // A withdrawal signature cannot be reused as a validator set update signature.
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), _digest(w));
        (, address[] memory nv) = _makeSet(3000, 4);
        vm.expectRevert();
        bridge.updateValidatorSet(nv, _equalPowers(4, 1), 2, sigs);
    }

    function testFuzz_ReplayProtection(uint64 id, uint256 amount) public {
        amount = bound(amount, 1, 1_000_000e6);
        Bridge.Withdrawal memory w = _w(id, bob, amount);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(3), _digest(w));
        bridge.withdraw(w, sigs);
        assertTrue(bridge.usedWithdrawal(id));
        vm.expectRevert(Bridge.WithdrawalAlreadyUsed.selector);
        bridge.withdraw(w, sigs);
        assertEq(usdc.balanceOf(bob), amount);
    }
}

contract BridgeRotationTest is BridgeTestBase {
    uint256[] internal newKeys;
    address[] internal newVals;

    function setUp() public override {
        super.setUp();
        (newKeys, newVals) = _makeSet(2000, 5);
    }

    function test_ValidatorSetRotation_Succeeds() public {
        uint256[] memory np = _equalPowers(5, 2);
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, 2);
        vm.expectEmit(true, false, false, true, address(bridge));
        emit Bridge.ValidatorSetUpdated(2, newVals, np);
        bridge.updateValidatorSet(newVals, np, 2, _signIdx(keys, _firstK(3), d));
        assertEq(bridge.epoch(), 2);
        assertEq(bridge.totalPower(), 10);
        assertEq(bridge.quorumPower(), 7);
        (address[] memory v,) = bridge.getValidators();
        assertEq(v.length, 5);
        for (uint256 i = 0; i < 4; i++) {
            assertEq(bridge.powerOf(vals[i]), 0);
        }
        // new set can withdraw at its own threshold (4 of 5 at power 2 = 8 >= 7)
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        bridge.withdraw(w, _signIdx(newKeys, _firstK(4), _digest(w)));
        assertEq(usdc.balanceOf(bob), 1e6);
    }

    function test_ValidatorSetRotation_BelowThresholdReverts() public {
        uint256[] memory np = _equalPowers(5, 1);
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, 2);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(2), d);
        vm.expectRevert(abi.encodeWithSelector(Bridge.InsufficientPower.selector, 2, 3));
        bridge.updateValidatorSet(newVals, np, 2, sigs);
    }

    function test_ValidatorSetRotation_NewSetCannotSelfAuthorize() public {
        uint256[] memory np = _equalPowers(5, 1);
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, 2);
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.updateValidatorSet(newVals, np, 2, _signIdx(newKeys, _firstK(5), d));
    }

    function test_ValidatorSetRotation_OldSetSignaturesRejectedAfterRotation() public {
        // Old set signs a withdrawal before the rotation, relayer is too late.
        Bridge.Withdrawal memory w = _w(5, bob, 1e6);
        Bridge.Sig[] memory oldSigs = _signIdx(keys, _firstK(4), _digest(w));
        _rotate(keys, newVals, _equalPowers(5, 1), 2);
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.withdraw(w, oldSigs);
        assertFalse(bridge.usedWithdrawal(5));
        // the new set can still process the same id
        bridge.withdraw(w, _signIdx(newKeys, _firstK(4), _digest(w)));
        assertEq(usdc.balanceOf(bob), 1e6);
    }

    function test_ValidatorSetRotation_OldSetCannotRotateAgain() public {
        _rotate(keys, newVals, _equalPowers(5, 1), 2);
        (, address[] memory third) = _makeSet(4000, 4);
        bytes32 d = bridge.hashValidatorSetUpdate(third, _equalPowers(4, 1), 3);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), d);
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.updateValidatorSet(third, _equalPowers(4, 1), 3, sigs);
    }

    function test_ValidatorSetRotation_EpochReplayRejected() public {
        uint256[] memory np = _equalPowers(5, 1);
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, 2);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), d);
        bridge.updateValidatorSet(newVals, np, 2, sigs);
        // exact replay of the same message
        vm.expectRevert(Bridge.StaleEpoch.selector);
        bridge.updateValidatorSet(newVals, np, 2, sigs);
    }

    function test_ValidatorSetRotation_EpochMustIncrease() public {
        uint256[] memory np = _equalPowers(5, 1);
        for (uint64 e = 0; e <= 1; e++) {
            bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, e);
            Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), d);
            vm.expectRevert(Bridge.StaleEpoch.selector);
            bridge.updateValidatorSet(newVals, np, e, sigs);
        }
        // epochs can skip ahead
        _rotate(keys, newVals, np, 10);
        assertEq(bridge.epoch(), 10);
    }

    function test_ValidatorSetRotation_ReplayOfOldRotationAfterRoundTrip() public {
        // A -> B (epoch 2) -> A (epoch 3); the original A->B message (epoch 2) cannot be replayed.
        uint256[] memory np = _equalPowers(5, 1);
        bytes32 d2 = bridge.hashValidatorSetUpdate(newVals, np, 2);
        Bridge.Sig[] memory sigs2 = _signIdx(keys, _firstK(4), d2);
        bridge.updateValidatorSet(newVals, np, 2, sigs2);
        _rotate(newKeys, vals, powers, 3);
        assertEq(bridge.epoch(), 3);
        vm.expectRevert(Bridge.StaleEpoch.selector);
        bridge.updateValidatorSet(newVals, np, 2, sigs2);
    }

    function test_ValidatorSetRotation_SignatureBindsPayload() public {
        uint256[] memory np = _equalPowers(5, 1);
        bytes32 d = bridge.hashValidatorSetUpdate(newVals, np, 2);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), d);
        // different powers
        vm.expectRevert();
        bridge.updateValidatorSet(newVals, _equalPowers(5, 100), 2, sigs);
        // different epoch
        vm.expectRevert();
        bridge.updateValidatorSet(newVals, np, 3, sigs);
        // different member list
        address[] memory alt = new address[](5);
        for (uint256 i = 0; i < 5; i++) {
            alt[i] = newVals[i];
        }
        alt[4] = address(uint160(alt[4]) + 1);
        vm.expectRevert();
        bridge.updateValidatorSet(alt, np, 2, sigs);
        assertEq(bridge.epoch(), 1);
    }

    function test_ValidatorSetRotation_InvalidNewSetReverts() public {
        address[] memory bad = new address[](2);
        bad[0] = newVals[1];
        bad[1] = newVals[0]; // unsorted
        uint256[] memory np = _equalPowers(2, 1);
        bytes32 d = bridge.hashValidatorSetUpdate(bad, np, 2);
        Bridge.Sig[] memory sigs = _signIdx(keys, _firstK(4), d);
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        bridge.updateValidatorSet(bad, np, 2, sigs);
        // length mismatch
        uint256[] memory np3 = _equalPowers(3, 1);
        d = bridge.hashValidatorSetUpdate(newVals, np3, 2);
        sigs = _signIdx(keys, _firstK(4), d);
        vm.expectRevert(Bridge.InvalidValidatorSet.selector);
        bridge.updateValidatorSet(newVals, np3, 2, sigs);
    }

    function test_ValidatorSetRotation_PartialOverlapKeepsContinuingMembers() public {
        // new set = old[0], old[1] + 2 new members; continuing members' withdrawal
        // signatures still count, retired members' do not.
        address[] memory nv = new address[](4);
        uint256[] memory nk = new uint256[](4);
        nv[0] = vals[0];
        nk[0] = keys[0];
        nv[1] = vals[1];
        nk[1] = keys[1];
        nv[2] = newVals[0];
        nk[2] = newKeys[0];
        nv[3] = newVals[1];
        nk[3] = newKeys[1];
        for (uint256 i = 1; i < 4; i++) {
            for (uint256 j = i; j > 0 && nv[j - 1] > nv[j]; j--) {
                (nv[j - 1], nv[j]) = (nv[j], nv[j - 1]);
                (nk[j - 1], nk[j]) = (nk[j], nk[j - 1]);
            }
        }
        _rotate(keys, nv, _equalPowers(4, 1), 2);
        Bridge.Withdrawal memory w = _w(1, bob, 1e6);
        bytes32 d = _digest(w);
        // old set's first 3 (includes retired vals[2]) -> NotValidator
        vm.expectRevert(Bridge.NotValidator.selector);
        bridge.withdraw(w, _signIdx(keys, _firstK(3), d));
        bridge.withdraw(w, _signIdx(nk, _firstK(3), d));
        assertEq(usdc.balanceOf(bob), 1e6);
    }

    function testFuzz_ValidatorSetRotation_Epochs(uint64 e1, uint64 e2) public {
        e1 = uint64(bound(e1, 2, type(uint64).max - 1));
        e2 = uint64(bound(e2, 0, type(uint64).max));
        _rotate(keys, newVals, _equalPowers(5, 1), e1);
        bytes32 d = bridge.hashValidatorSetUpdate(vals, powers, e2);
        Bridge.Sig[] memory sigs = _signIdx(newKeys, _firstK(5), d);
        if (e2 <= e1) {
            vm.expectRevert(Bridge.StaleEpoch.selector);
            bridge.updateValidatorSet(vals, powers, e2, sigs);
        } else {
            bridge.updateValidatorSet(vals, powers, e2, sigs);
            assertEq(bridge.epoch(), e2);
        }
    }
}
