# Polkadot AnV: dealing with timeouts

We present a proposal to deal with timeouts when executing parablocks in AnV, that only executes Wasmtime, never Wasmi. 

**Warning.** We need to assume that even though validators can have considerably different execution times for the same parablock, the *ratio* of execution times stays roughly constant; i.e. if validator $v$ executes some parablock $B$ twice as fast as validator $v'$, then $v$ would also be roughly twice as fast as $v'$ when executing any other parablock $B'$ in any other parachain. We also need to assume that, for a fixed parablock $B$, the *average* processing time among all validators evolves slowly (hence not validators should not all  update their machines at the same time).

## Definitions and intuition

Every validator $v$ keeps track of an estimate $r_v$ of the ratio between her own execution time and the average of the claimed execution time of all validators, if all validators executed the same parablock. So, $r_v>1$ means that validator $v$ is slower than average, and vice versa. This estimate is updated by $v$ after each block.

When a validator executes a parablock $B$, it reports her execution time $t_v(B)$ in milliseconds along with her current estimated ratio $r_v$. From the reported pair $(t_v(B), r_v)$ we learn that $v$'s estimate of the *average execution time* $t_{average}(B)$ of this parablock is $t_v(B)/r_v$. A validator should accept parablock $B$ only if her estimate of $t_{average}(B)$ is less than a certain threshold, and this threshold is lower for backing validators and higher for checking validators, to minimize the possibility of conflict (i.e. a checker that reports "timeout" on a backed block). In case of conflict, we escalate and ask all validators to compute their own estimates of $t_{average}(B)$. We slash validators whose estimates are outliers in the corresponding distribution, for failing to estimate $t_{average}(B)$ accurately.

There are two constants $\alpha$ and $\beta$ (say, $\alpha=2$ and $\beta=3$), and two time limits $t_{back}$ and $t_{check}=\alpha^2\beta \cdot t_{back}$, expressed in milliseconds. Parameters $\alpha$, $\beta$ and $t_{back}$ are decided by governance. The intuition behind constant $\alpha$ and $\beta$ is that 
 * a validator $v$ gets slashed only if her reported estimate of $t_{average}(B)$ is more an a multiplicative factor $\alpha$ away from the estimates of at least two thirds of the validators, and 
*  as long as all honest validators have reported estimates less than a multiplicative factor $\beta$ away from one another, an escalation will always lead to someone getting slashed.

We ask checkers to make separate reports for timeout and for invalidity (without timeout), to better understand a slashing event. However we treat these two reports equally in our protocol logic.

## Protocol description

**Backers.** A backer $v$ records her execution time $t_v(B)$ on parablock $B$ and backs it if $t_v(B)/r_v \leq t_{back}$. 

**Checkers.** A checker $v$ records her execution time $t_v(B)$ on $B$.
* (Validity) If $t_v(B)/r_v<t_{check}$, she approves it and reports $(t_v(B), r_v)$.
* (Timeout) If it takes longer than this, she aborts it and reports "timeout".
* (Invalidity) If $B$ is invalid, she reports "invalid".

**Regular case.** Let $C(B)$ be the set of checkers of parablock $B$. If all checkers in $C(B)$ report validity, $B$ is accepted. Let $t_{C(B)}$ be the median of the set $\{t_v(B)/r_v: \ v\in C(B)\}$. If $t_{C(B)}< \alpha\beta\cdot t_{check}$, backers are paid as usual, but if $t_{C(B)}\geq \alpha\beta\cdot t_{check}$ we don't pay the backers, as a punishment for backing slow blocks. Additionally, as a light punishment we can issue "admonishments" to the backers, which do nothing technically buy may affect their reputation negatively vis-à-vis nominators. We consider here that escalation is not worth the effort, but at the same time slashing is not fair without escalation.

**Escalation case.** If one or more checkers in $C(B)$ report timeout or invalidity, we escalate and ask all other validators to check $B$ and report. Original backers and checkers of $B$ cannot report again. We then consider the set of reported values $\{t_v(B)/r_v\}$, where for backers we consider their reported value to be $t_{back}$, while for reporters of timeout and invalidity we consider their reported value to be $t_{check}$. This corresponds to the distribution of validators' estimates of the average execution time $t_{average}(B)$. See the figure below.

![](https://i.imgur.com/LE8GcD2.jpg)

a. Let $n$ be the total number of validators in Polkadot. If less than $2n/3$ validators report in total (within some established time window), reject the block and continue (skip steps below without slashing anyone).

b. Sort the reported values $\{t_v(B)/r_v\}$ from low to high, and let $t_{low}$ be the value at position $2n/3$ counting from the right. Slash 100% any validator whose reported value is less than or equal to $t_{low}/\alpha$. Notice in particular that if $t_{low}\geq \alpha\cdot t_{back}$ then all backers will be slashed.

c. Let $t_{high}$ be the value in $\{t_v(B)/r_v\}$ at position $2n/3$ from the left, and slash any validator whose reported value is greater than or equal to $\alpha \cdot t_{high}$. This slash does not need to be 100%, maybe 10% is enough, because misreporting a valid block as invalid constitutes a griefing attack but not a security attack. Notice in particular that if $t_{high}\leq \alpha\beta\cdot t_{back}$, then all timeout and invalidity reporters will be slashed.

d. If $t_{low}< \alpha\cdot t_{back}$ and $t_{high}\leq \alpha\beta\cdot t_{back}$ then accept the block, otherwise reject it.

## Protocol properties

* From points b and c above, it is clear that a reporter gets slashed only if her reported estimate of $t_{average}(B)$ is at least a multiplicative factor $\alpha$ away from at least two thirds of validators.
* Assuming that at most one third of validators are malicious, both $t_{low}$ and $t_{high}$ correspond to estimates of $t_{average}(B)$ by honest validators. And if honest estimates are clustered enough so that the ratio $t_{high}/t_{low}$ is bounded by $\beta$, then the escalation case will always slash someone (in point b or c, or both).
* Block $B$ is accepted only if all invalidity and timeout reporters are slashed and none of the backers is slashed.
* The protocol follows the principle that if the number of reporters decreases then the number or slashing events also decreases, and the likelihood of the block being accepted decreases as well. In the limit when there are fewer than $2n/3$ reports (case a), we reject the block and slash no one.
* Whenever we reject the block and slash no one, the protocol should raise a red flag and governance should jump in. This can only occur if there are fewer than $2n/3$ reports or if $t_{high}/t_{low}>\beta$. In the latter case we should increase $\beta$, or better yet find a way to reduce the variance in execution times (see our warning at the beginning of the note).


## Updating the estimates $r_v$ 

After each parablock, if $v$ is a backer or a checker of parablock $B$, she updates her estimate $r_v$ as

$$r_v\leftarrow (1-\varepsilon)r_v + \varepsilon\cdot \frac{t_v(B)}{median\{t_{v'}(B): \ v'\in C(B)\}},$$

where $\varepsilon$ is a small constant set by governance (say $\varepsilon=0.05$) and we recall that $C(B)$ is the set of verifiers of parablock $B$. Notice that this is an exponential moving average over the series of observations $\frac{t_v(B)}{median\{t_{v'}(B): \ v'\in Ver(B)\}}$ for the different parablocks $B$ that validator $v$ is assigned to, and that for any single parablock $B$ the value $\frac{t_v(B)}{median\{t_{v'}(B): \ v'\in Ver(B)\}}$ constitutes the best estimate of $r_v$ available to $v$. We take the median on the denominator (as opposed to the average) as added protection against sporadic checkers that report wrong execution times, either accidentally or maliciously.

We remark that an adversarial checker cannot lie "for free" on her reported ratio $t_v(B)/r_v$, as she runs the risk of getting slashed. However, she can still lie for free about both her execution time $t_v(B)$ and her value of $r_v$ and scale by a common arbitrary factor. To limit this attack, we can also slash a validator whose reported estimate $r_v$ is too far from 1 (say, it should be between 0.1 and 10), or varies too much too quickly. Hence, validators would need to take preventive measures to keep their node speeds always close to the average.