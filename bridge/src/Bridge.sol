// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

interface IERC20Minimal {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @title hlbft USDC bridge
/// @notice Deposits: users lock USDC and get a monotone `depositId`; hlbft
///         validators attest the `Deposit` event and credit the user on L1.
///         Withdrawals: hlbft validators sign an EIP-712 `Withdrawal`; anyone
///         (the relayer) submits it with signatures carrying a quorum of the
///         contract's current validator power.
///
/// Quorum (identical to the L1, `ValidatorSet::quorum_stake`):
///     quorum = floor(2 * totalPower / 3) + 1
/// i.e. the smallest integer strictly greater than 2/3 of the total power.
///
/// Signature list rules (withdraw and updateValidatorSet):
///   * every signature must recover (ecrecover != 0) with v in {27, 28} and a
///     low-s value (s <= secp256k1n/2, EIP-2), so signatures are not malleable;
///   * recovered signers must be strictly ascending (=> unique);
///   * every signer must be a member of the current validator set;
///   * the summed power of signers must be >= quorum.
///
/// Replay protection: `usedWithdrawal[id]` for withdrawals; validator-set
/// updates require a strictly increasing epoch. The EIP-712 domain binds
/// chainId and this contract's address. Withdrawals are always checked against
/// the *current* set, so signatures from retired validators stop counting at
/// rotation.
contract Bridge {
    // ------------------------------------------------------------------ types
    struct Withdrawal {
        uint64 id;
        address destination;
        uint256 amount;
    }

    struct Sig {
        uint8 v;
        bytes32 r;
        bytes32 s;
    }

    // -------------------------------------------------------------- constants
    bytes32 public constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    bytes32 public constant WITHDRAWAL_TYPEHASH = keccak256("Withdrawal(uint64 id,address destination,uint256 amount)");
    bytes32 public constant VALIDATOR_SET_UPDATE_TYPEHASH =
        keccak256("ValidatorSetUpdate(bytes32 validatorsHash,bytes32 powersHash,uint64 epoch)");
    bytes32 private constant NAME_HASH = keccak256("HlbftBridge");
    bytes32 private constant VERSION_HASH = keccak256("1");

    /// secp256k1n / 2
    uint256 private constant HALF_N = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;
    uint256 public constant MAX_VALIDATORS = 64;
    /// Deposits must fit the L1's signed 64-bit 1e-6 USD amount.
    uint256 public constant MAX_DEPOSIT = uint256(type(uint64).max >> 1);

    // ------------------------------------------------------------------ state
    IERC20Minimal public immutable token;
    uint256 public immutable minDeposit;

    /// Id assigned to the next deposit (first deposit gets id 0).
    uint64 public nextDepositId;
    /// Epoch of the current validator set.
    uint64 public epoch;
    uint256 public totalPower;
    address[] private _validators;
    mapping(address => uint256) public powerOf;
    mapping(uint64 => bool) public usedWithdrawal;

    // ----------------------------------------------------------------- events
    event Deposit(uint64 indexed id, address indexed user, uint256 amount);
    event Withdrawn(uint64 indexed id, address indexed destination, uint256 amount);
    event ValidatorSetUpdated(uint64 indexed epoch, address[] validators, uint256[] powers);

    // ----------------------------------------------------------------- errors
    error DepositTooSmall();
    error DepositTooLarge();
    error TransferFailed();
    error WithdrawalAlreadyUsed();
    error InvalidWithdrawal();
    error InvalidSignature();
    error SignersNotSorted();
    error NotValidator();
    error InsufficientPower(uint256 have, uint256 need);
    error StaleEpoch();
    error InvalidValidatorSet();
    error InvalidToken();

    constructor(
        IERC20Minimal token_,
        uint256 minDeposit_,
        address[] memory validators_,
        uint256[] memory powers_,
        uint64 epoch_
    ) {
        if (address(token_).code.length == 0) revert InvalidToken();
        token = token_;
        minDeposit = minDeposit_ == 0 ? 1 : minDeposit_;
        _setValidators(validators_, powers_, epoch_);
    }

    // --------------------------------------------------------------- deposits
    /// @notice Lock `amount` USDC (6 decimals). Requires prior `approve`.
    function deposit(uint256 amount) external returns (uint64 id) {
        if (amount < minDeposit) revert DepositTooSmall();
        if (amount > MAX_DEPOSIT) revert DepositTooLarge();
        id = nextDepositId++;
        _call(abi.encodeCall(IERC20Minimal.transferFrom, (msg.sender, address(this), amount)));
        emit Deposit(id, msg.sender, amount);
    }

    // ------------------------------------------------------------ withdrawals
    function withdraw(Withdrawal calldata w, Sig[] calldata sigs) external {
        if (usedWithdrawal[w.id]) revert WithdrawalAlreadyUsed();
        if (w.destination == address(0) || w.amount == 0) revert InvalidWithdrawal();
        _checkQuorum(hashWithdrawal(w), sigs);
        usedWithdrawal[w.id] = true;
        _call(abi.encodeCall(IERC20Minimal.transfer, (w.destination, w.amount)));
        emit Withdrawn(w.id, w.destination, w.amount);
    }

    // ------------------------------------------------------- validator rotation
    /// @notice Replace the validator set. `newValidators` must be strictly
    ///         ascending, non-zero, with non-zero powers. Signed by the current set.
    function updateValidatorSet(
        address[] calldata newValidators,
        uint256[] calldata newPowers,
        uint64 newEpoch,
        Sig[] calldata sigs
    ) external {
        if (newEpoch <= epoch) revert StaleEpoch();
        _checkQuorum(hashValidatorSetUpdate(newValidators, newPowers, newEpoch), sigs);
        for (uint256 i = 0; i < _validators.length; i++) {
            delete powerOf[_validators[i]];
        }
        delete _validators;
        _setValidators(newValidators, newPowers, newEpoch);
    }

    // ------------------------------------------------------------------ views
    function domainSeparator() public view returns (bytes32) {
        return keccak256(abi.encode(DOMAIN_TYPEHASH, NAME_HASH, VERSION_HASH, block.chainid, address(this)));
    }

    function hashWithdrawal(Withdrawal calldata w) public view returns (bytes32) {
        bytes32 structHash = keccak256(abi.encode(WITHDRAWAL_TYPEHASH, w.id, w.destination, w.amount));
        return _typedDigest(structHash);
    }

    function hashValidatorSetUpdate(address[] calldata newValidators, uint256[] calldata newPowers, uint64 newEpoch)
        public
        view
        returns (bytes32)
    {
        bytes32 structHash = keccak256(
            abi.encode(
                VALIDATOR_SET_UPDATE_TYPEHASH,
                keccak256(abi.encodePacked(newValidators)),
                keccak256(abi.encodePacked(newPowers)),
                newEpoch
            )
        );
        return _typedDigest(structHash);
    }

    function getValidators() external view returns (address[] memory validators, uint256[] memory powers) {
        validators = _validators;
        powers = new uint256[](validators.length);
        for (uint256 i = 0; i < validators.length; i++) {
            powers[i] = powerOf[validators[i]];
        }
    }

    /// @notice Minimum signer power: floor(2 * totalPower / 3) + 1.
    function quorumPower() public view returns (uint256) {
        return (totalPower * 2) / 3 + 1;
    }

    // --------------------------------------------------------------- internal
    function _typedDigest(bytes32 structHash) internal view returns (bytes32) {
        return keccak256(abi.encodePacked(hex"1901", domainSeparator(), structHash));
    }

    function _checkQuorum(bytes32 digest, Sig[] calldata sigs) internal view {
        uint256 power = 0;
        address last = address(0);
        for (uint256 i = 0; i < sigs.length; i++) {
            Sig calldata sg = sigs[i];
            if ((sg.v != 27 && sg.v != 28) || uint256(sg.s) > HALF_N || sg.r == 0 || sg.s == 0) {
                revert InvalidSignature();
            }
            address signer = ecrecover(digest, sg.v, sg.r, sg.s);
            if (signer == address(0)) revert InvalidSignature();
            if (signer <= last) revert SignersNotSorted();
            last = signer;
            uint256 p = powerOf[signer];
            if (p == 0) revert NotValidator();
            power += p;
        }
        uint256 need = quorumPower();
        if (power < need) revert InsufficientPower(power, need);
    }

    function _setValidators(address[] memory vals, uint256[] memory powers, uint64 newEpoch) internal {
        uint256 n = vals.length;
        if (n == 0 || n > MAX_VALIDATORS || n != powers.length) revert InvalidValidatorSet();
        uint256 total = 0;
        address last = address(0);
        for (uint256 i = 0; i < n; i++) {
            // strictly ascending also rules out address(0) and duplicates
            if (vals[i] <= last || powers[i] == 0 || powers[i] > type(uint64).max) revert InvalidValidatorSet();
            last = vals[i];
            powerOf[vals[i]] = powers[i];
            _validators.push(vals[i]);
            total += powers[i];
        }
        totalPower = total;
        epoch = newEpoch;
        emit ValidatorSetUpdated(newEpoch, vals, powers);
    }

    function _call(bytes memory data) internal {
        (bool ok, bytes memory ret) = address(token).call(data);
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TransferFailed();
    }
}
