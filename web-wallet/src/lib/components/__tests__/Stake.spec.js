import {
	afterAll,
	afterEach,
	describe,
	expect,
	it,
	vi
} from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/svelte";

import { deductLuxFeeFrom } from "$lib/contracts";
import { createCurrencyFormatter } from "$lib/dusk/currency";

import { Stake } from "..";
import { tick } from "svelte";

describe("Stake", () => {
	const formatter = createCurrencyFormatter("en", "DUSK", 9);
	const lastTxId = "some-id";

	const baseProps = {
		execute: vi.fn().mockResolvedValue(lastTxId),

		/** @type {StakeType} */
		flow: "stake",
		formatter,
		gasSettings: {
			gasLimit: 20000000,
			gasLimitLower: 10000000,
			gasLimitUpper: 1000000000,
			gasPrice: 1,
			gasPriceLower: 1
		},
		rewards: 345,
		spendable: 10000,
		staked: 278,
		statuses: [{
			label: "Spendable",
			value: "1,000.000000000"
		}, {
			label: "Total Locked",
			value: "278.000000000"
		}, {
			label: "Rewards",
			value: "345.000000000"
		}]
	};
	const baseOptions = {
		props: baseProps,
		target: document.body
	};

	const maxSpendable = deductLuxFeeFrom(
		baseProps.spendable,
		baseProps.gasSettings.gasPrice * baseProps.gasSettings.gasLimit
	);

	afterEach(() => {
		cleanup();
		baseProps.execute.mockClear();
	});

	it("should render the Stake component", () => {
		const { container, getByRole } = render(Stake, baseOptions);
		const nextButton = getByRole("button", { name: "Next" });
		const amountInput = getByRole("spinbutton");

		expect(nextButton).toBeEnabled();
		expect(amountInput.getAttribute("min")).toBe("1000");
		expect(amountInput.getAttribute("max")).toBe(maxSpendable.toString());
		expect(container.firstChild).toMatchSnapshot();
	});

	it("should disable the next button if the stake amount is invalid on mount", async () => {
		const props = {
			...baseProps,
			gasSettings: {
				...baseProps.gasSettings,
				gasLimit: 40000000,
				gasPrice: 40000000
			}
		};
		const currentMaxSpendable = deductLuxFeeFrom(
			props.spendable,
			props.gasSettings.gasPrice * props.gasSettings.gasLimit
		);
		const { getByRole } = render(Stake, { ...baseOptions, props });
		const nextButton = getByRole("button", { name: "Next" });
		const amountInput = getByRole("spinbutton");

		await tick();
		expect(nextButton).toBeDisabled();
		expect(amountInput.getAttribute("min")).toBe("1000");
		expect(amountInput.getAttribute("max")).toBe(currentMaxSpendable.toString());
	});

	it("should set the max amount in the textbox if the user clicks the related button", async () => {
		const { getByRole } = render(Stake, baseOptions);
		const useMaxButton = getByRole("button", { name: "USE MAX" });

		await fireEvent.click(useMaxButton);

		const amountInput = getByRole("spinbutton");

		expect(amountInput).toHaveValue(maxSpendable);
	});

	it("should disable the next button if the user enters an invalid amount", async () => {
		const { getByRole } = render(Stake, baseOptions);
		const nextButton = getByRole("button", { name: "Next" });
		const amountInput = getByRole("spinbutton");

		expect(nextButton).toBeEnabled();

		await fireEvent.input(amountInput, { target: { value: baseProps.spendable * 2 } });

		expect(nextButton).toBeDisabled();
	});

	it("should render the review step of the Stake component", async () => {
		const { container, getByRole } = render(Stake, baseOptions);

		await fireEvent.click(getByRole("button", { name: "Next" }));

		expect(container.firstChild).toMatchSnapshot();
	});

	describe("Stake operation", () => {
		vi.useFakeTimers();

		const expectedExplorerLink = `https://explorer.dusk.network/transactions/transaction?id=${lastTxId}`;

		afterAll(() => {
			vi.useRealTimers();
		});

		it("should perform a stake for the desired amount, give a success message and supply a link to see the transaction in the explorer", async () => {
			const { getByRole, getByText } = render(Stake, baseProps);
			const amountInput = getByRole("spinbutton");

			expect(amountInput).toHaveValue(1000);

			await fireEvent.click(getByRole("button", { name: "Next" }));
			await fireEvent.click(getByRole("button", { name: "STAKE" }));

			await vi.advanceTimersToNextTimerAsync();

			expect(baseProps.execute).toHaveBeenCalledTimes(1);
			expect(baseProps.execute).toHaveBeenCalledWith(1000);

			const explorerLink = getByRole("link", { name: /explorer/i });

			expect(getByText("Transaction completed")).toBeInTheDocument();
			expect(explorerLink).toHaveAttribute("target", "_blank");
			expect(explorerLink).toHaveAttribute("href", expectedExplorerLink);
		});

		it("should show an error message if the transfer fails", async () => {
			const errorMessage = "Some error message";

			baseProps.execute.mockRejectedValueOnce(new Error(errorMessage));

			const { getByRole, getByText } = render(Stake, baseProps);
			const amountInput = getByRole("spinbutton");

			await fireEvent.input(amountInput, { target: { value: 2567 } });
			await fireEvent.click(getByRole("button", { name: "Next" }));
			await fireEvent.click(getByRole("button", { name: "STAKE" }));

			await vi.advanceTimersToNextTimerAsync();

			expect(baseProps.execute).toHaveBeenCalledTimes(1);
			expect(baseProps.execute).toHaveBeenCalledWith(2567);
			expect(getByText("Transaction failed")).toBeInTheDocument();
			expect(getByText(errorMessage)).toBeInTheDocument();
		});

		it("should show the success message, but no explorer link, if the execution promise doesn't resolve with an hash", async () => {
			baseProps.execute.mockResolvedValueOnce(void 0);

			const { getByRole, getByText } = render(Stake, baseProps);

			await fireEvent.click(getByRole("button", { name: "USE MAX" }));
			await fireEvent.click(getByRole("button", { name: "Next" }));
			await fireEvent.click(getByRole("button", { name: "STAKE" }));

			await vi.advanceTimersToNextTimerAsync();

			expect(baseProps.execute).toHaveBeenCalledTimes(1);
			expect(baseProps.execute).toHaveBeenCalledWith(maxSpendable);
			expect(getByText("Transaction completed")).toBeInTheDocument();
			expect(() => getByRole("link", { name: /explorer/i })).toThrow();
		});
	});
});
