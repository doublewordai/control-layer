export const formatDollars = (amount: number, maxDecimalPlaces: number = 2) => {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: maxDecimalPlaces,
    currencyDisplay: "symbol",
  }).format(amount);
};
