import { keccak256 } from 'viem';

const target = '0xd70a0e30';
const encoder = new TextEncoder();

// Extended list of error signatures
const errors = [
  'ConditionNotFinished()',
  'ConditionNotResolved()',
  'ConditionNotResolved(uint256)',
  'GameNotFinished()',
  'BetNotExists()',
  'BetNotExists(uint256)',
  'AlreadyPaid()',
  'AlreadyPaid(uint256)',
  'CoreNotActive()',
  'LockedBetToken(uint256)',
  'NotResolved()',
  'ConditionNotCreated()',
  'GameCanceled()',
  'ConditionAlreadyResolved()',
  'InvalidTokenId()',
  'BetNotResolved()',
  'NotFinished()',
  'NoPayout()',
  'WrongOutcome()',
  'NotOwner()',
  'ConditionStopped()',
  'ConditionCanceled()',
  'ConditionResolved()',
  'ConditionAlreadyCreated()',
  'ClaimTimeout()',
  'ClaimAlreadyPaid()',
  'BetAlreadyPaid()',
  'Interrupted()',
  'GameAlreadyCanceled()',
  'CantResolve()',
  'WrongToken()',
  'OnlyCore()',
  'OnlyLP()',
  'OnlyOracle()',
  'BetExpired()',
  'Paused()',
  'NotActiveBet()',
  'BetNotRedeemable()',
  'ConditionNotSettled()',
  'ConditionResolved(uint256)',
  'BetCanceled()',
  'GameNotResolved()',
  'GameStarted()',
  'InsufficientFund()',
  'InsufficientFunds()',
  'SmallBet()',
  'BigBet()',
  'SmallOdds()',
  'OddsTooBig()',
];

let found = false;
for (const sig of errors) {
  const h = keccak256(encoder.encode(sig));
  const sel = h.slice(0, 10);
  if (sel === target) {
    console.log(`MATCH: ${sig} => ${sel}`);
    found = true;
  }
}
if (!found) {
  console.log(`No match found for ${target}`);
  console.log('Trying 4byte.directory...');
}
